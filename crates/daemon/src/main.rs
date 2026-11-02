//! rustyclaude-daemon: keeps a machine's Claude Code sessions running in tmux and
//! exposes them to the RustyClaude app.
//!
//! - Sessions are listed in `~/.config/rustyclaude/daemon.toml` (editable from the app).
//!   Each runs as a normal tmux session with the same name (`tmux attach -t 21wild`),
//!   is started at boot, restarted if it exits, and runs with `--remote-control`.
//! - Each managed session gets its own Claude config dir (`~/.claude-sessions/<name>`),
//!   so its login persists and `/login` in one session never affects another. Shared
//!   settings (settings.json, CLAUDE.md, agents, ...) are symlinked from `~/.claude`.
//! - tmux sessions started by hand are listed too and can be adopted or moved.
//! - Logging in / moving accounts is done by driving `/login` inside the session.

mod tmux;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use rustyclaude_protocol::{self as proto, ClientMsg, DaemonMsg, SessionInfo, SessionMode};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
};
use tokio_tungstenite::tungstenite::Message;

static LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)usage limit|limit reached|limit will reset|out of extra usage|reached your [a-z ]*limit").unwrap()
});
static OAUTH_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https://\S*oauth\S*").unwrap());
static LOGIN_METHOD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)login method").unwrap());
static NEEDS_LOGIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)login method|please run /login|not logged in|invalid api key|oauth token (has )?expired").unwrap()
});
static LOGIN_OK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)login successful").unwrap());
static PASTE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)paste code").unwrap());
static LOGIN_FAILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)oauth error|login failed|invalid (authorization )?code").unwrap());
/// --dangerously-skip-permissions confirmation ("No, exit" is the default!).
static BYPASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)yes, i accept").unwrap());
/// First-run / post-login prompts where Enter takes the sensible default.
static ENTER_PROMPTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)press enter to continue|choose the text style|do you trust the files|yes, proceed|security notes").unwrap()
});

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const RESTART_BACKOFF: Duration = Duration::from_secs(15);
const TICK: Duration = Duration::from_millis(800);
/// Scrollback scanned each tick. Prompts are recognised by their count going up, so
/// text left on screen from earlier logins never triggers anything.
const HISTORY: u32 = 1500;
/// Linked from ~/.claude into each session's own config dir. Conversation history
/// (`projects`, …) is shared so sessions resume your existing conversations and keep
/// them when moved between accounts.
const SHARED: &[&str] = &[
    "projects",
    "todos",
    "file-history",
    "history.jsonl",
    "settings.json",
    "CLAUDE.md",
    "agents",
    "commands",
    "skills",
    "plugins",
    "output-styles",
    "hooks",
    "keybindings.json",
];

#[derive(Parser)]
struct Args {
    /// Address to listen on: the machine's LAN or Tailscale IP.
    #[arg(long, default_value = "127.0.0.1:7878")]
    bind: String,
    /// Shared secret clients must present.
    #[arg(long, env = "RUSTYCLAUDE_TOKEN")]
    token: String,
    /// Claude Code executable.
    #[arg(long, default_value = "claude")]
    claude: String,
    /// tmux executable.
    #[arg(long, default_value = "tmux")]
    tmux: String,
    /// Session list (default: ~/.config/rustyclaude/daemon.toml).
    #[arg(long)]
    config: Option<PathBuf>,
}

// ---- persisted config ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionDef {
    name: String,
    dir: String,
    /// App account label of the last successful login.
    #[serde(default)]
    account: Option<String>,
    #[serde(default = "yes")]
    autostart: bool,
    /// Set when stopped from the app; autostart resumes after an explicit start.
    #[serde(default)]
    stopped: bool,
    /// Own config dir (~/.claude-sessions/<name>). False for adopted sessions that
    /// keep using whatever config they were started with.
    #[serde(default = "yes")]
    isolated: bool,
    /// Start with --continue when the folder has a previous conversation.
    #[serde(default = "yes")]
    resume: bool,
    #[serde(default)]
    skip_permissions: bool,
    /// Any other flags for `claude`.
    #[serde(default)]
    extra_args: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DaemonConfig {
    #[serde(default, rename = "session")]
    sessions: Vec<SessionDef>,
}

// ---- runtime state -------------------------------------------------------------

/// How many times each pattern appears in the scrollback.
#[derive(Clone, Copy, Default)]
struct Counts {
    method: usize,
    prompts: usize,
    ok: usize,
    paste: usize,
    failed: usize,
    limit: usize,
}

impl Counts {
    fn of(screen: &str) -> Self {
        // Count lines, not matches: one prompt line can contain several phrases.
        let n = |re: &Regex| screen.lines().filter(|l| re.is_match(l)).count();
        Counts {
            method: n(&LOGIN_METHOD),
            prompts: n(&ENTER_PROMPTS),
            ok: n(&LOGIN_OK),
            paste: n(&PASTE_CODE),
            failed: n(&LOGIN_FAILED),
            limit: n(&LIMIT),
        }
    }
}

struct Login {
    account: String,
    started: Instant,
    baseline_urls: HashSet<String>,
    /// Counts when the login began; only occurrences beyond these are acted on.
    base: Counts,
    /// Enter presses already sent for method menus / prompts.
    method_enters: usize,
    prompt_enters: usize,
    url_sent: bool,
    code_prompted: bool,
}

#[derive(Default)]
struct Live {
    login: Option<Login>,
    limited: bool,
    /// Limit messages at or below this count are old (from before the last login).
    limit_base: usize,
    /// Prompt count when the post-login settling began, and Enters sent since.
    settle_base: usize,
    settle_enters: usize,
    needs_login: bool,
    last_start: Option<Instant>,
    /// After a login: keep accepting first-run prompts until then.
    settle_until: Option<Instant>,
    last_settle_enter: Option<Instant>,
    /// Started without --remote-control (no login yet); enable it once logged in.
    rc_pending: bool,
    /// Skip-permissions confirmations already answered (by line count in scrollback).
    bypass_answered: usize,
}

struct State {
    args: Args,
    host: String,
    home: PathBuf,
    config_path: PathBuf,
    cfg: Mutex<DaemonConfig>,
    live: Mutex<HashMap<String, Live>>,
    /// Latest view, pushed to clients when it changes.
    view: Mutex<Vec<SessionInfo>>,
    notify: broadcast::Sender<DaemonMsg>,
}

impl State {
    fn save(&self) {
        let cfg = self.cfg.lock().unwrap();
        let write = || -> Result<()> {
            std::fs::create_dir_all(self.config_path.parent().unwrap())?;
            let tmp = self.config_path.with_extension("toml.tmp");
            std::fs::write(&tmp, toml::to_string_pretty(&*cfg)?)?;
            std::fs::rename(tmp, &self.config_path)?;
            Ok(())
        };
        if let Err(e) = write() {
            eprintln!("saving {}: {e:#}", self.config_path.display());
        }
    }

    fn def(&self, name: &str) -> Option<SessionDef> {
        self.cfg.lock().unwrap().sessions.iter().find(|d| d.name == name).cloned()
    }

    fn update_def(&self, name: &str, f: impl FnOnce(&mut SessionDef)) -> bool {
        let found = {
            let mut cfg = self.cfg.lock().unwrap();
            cfg.sessions.iter_mut().find(|d| d.name == name).map(f).is_some()
        };
        if found {
            self.save();
        }
        found
    }

    fn status(&self, session: &str, status: &str, message: Option<String>) {
        let _ = self.notify.send(DaemonMsg::LoginStatus {
            session: session.into(),
            status: status.into(),
            message,
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tmux::init(&args.tmux);
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")?;
    let config_path = args.config.clone().unwrap_or_else(|| home.join(".config/rustyclaude/daemon.toml"));
    let cfg: DaemonConfig = match std::fs::read_to_string(&config_path) {
        Ok(t) => toml::from_str(&t).with_context(|| format!("parsing {}", config_path.display()))?,
        Err(_) => DaemonConfig::default(),
    };
    tmux::version().context("tmux not found; install it (e.g. apt install tmux)")?;

    let listener = TcpListener::bind(&args.bind).await?;
    eprintln!("rustyclaude-daemon listening on ws://{} ({} sessions configured)", args.bind, cfg.sessions.len());
    let state = Arc::new(State {
        host: gethostname::gethostname().to_string_lossy().into_owned(),
        args,
        home,
        config_path,
        cfg: Mutex::new(cfg),
        live: Mutex::new(HashMap::new()),
        view: Mutex::new(Vec::new()),
        notify: broadcast::channel(256).0,
    });

    let st = state.clone();
    std::thread::spawn(move || loop {
        supervise(&st);
        std::thread::sleep(TICK);
    });

    loop {
        let (stream, peer) = listener.accept().await?;
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, st).await {
                eprintln!("{peer}: {e:#}");
            }
        });
    }
}

// ---- sessions ------------------------------------------------------------------

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn expand(home: &Path, p: &str) -> PathBuf {
    match p.trim() {
        "" | "~" => home.to_path_buf(),
        c if c.starts_with("~/") => home.join(&c[2..]),
        c => PathBuf::from(c),
    }
}

fn session_config_dir(st: &State, def: &SessionDef) -> PathBuf {
    if def.isolated {
        st.home.join(".claude-sessions").join(&def.name)
    } else {
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| st.home.join(".claude"))
    }
}

fn has_credentials(dir: &Path) -> bool {
    dir.join(".credentials.json").exists()
}

/// Creates the per-session config dir and links shared settings from ~/.claude.
fn prepare_config_dir(st: &State, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let shared = st.home.join(".claude");
    for item in SHARED {
        let src = shared.join(item);
        let dst = dir.join(item);
        let dst_meta = std::fs::symlink_metadata(&dst).ok();
        if dst_meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        // Claude already created a real directory here (e.g. a conversation started
        // before history was shared): move its contents into the shared one first.
        if dst_meta.as_ref().is_some_and(|m| m.is_dir()) {
            std::fs::create_dir_all(&src)?;
            merge_dir(&dst, &src)?;
            std::fs::remove_dir_all(&dst)?;
        } else if dst_meta.is_some() {
            continue; // a real file: leave it alone
        }
        let is_dir_item = !item.contains('.');
        if !src.exists() && is_dir_item {
            std::fs::create_dir_all(&src)?;
        }
        if src.exists() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&src, &dst).with_context(|| format!("linking {}", dst.display()))?;
        }
    }
    Ok(())
}

/// Moves everything in `from` into `to`, recursing into directories; never overwrites.
fn merge_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            merge_dir(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::rename(entry.path(), &target).or_else(|_| std::fs::copy(entry.path(), &target).map(|_| ()))?;
        }
    }
    Ok(())
}

/// Claude Code stores a folder's conversations in projects/<path with non-alphanumerics as '-'>.
fn has_conversation(cfg_dir: &Path, dir: &Path) -> bool {
    let key: String = dir.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    std::fs::read_dir(cfg_dir.join("projects").join(key))
        .map(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl")))
        .unwrap_or(false)
}

fn start_session(st: &State, def: &SessionDef) -> Result<()> {
    let dir = expand(&st.home, &def.dir);
    if !dir.is_dir() {
        bail!("directory does not exist: {}", dir.display());
    }
    let cfg_dir = session_config_dir(st, def);
    if def.isolated {
        prepare_config_dir(st, &cfg_dir)?;
    }
    let logged_in = has_credentials(&cfg_dir);
    let mut cmd: Vec<String> = vec!["env".into()];
    if def.isolated {
        cmd.push(format!("CLAUDE_CONFIG_DIR={}", cfg_dir.display()));
    }
    cmd.push(st.args.claude.clone());
    if def.resume && has_conversation(&cfg_dir, &dir) {
        cmd.push("--continue".into());
    }
    if def.skip_permissions {
        cmd.push("--dangerously-skip-permissions".into());
    }
    cmd.extend(def.extra_args.iter().cloned());
    if logged_in {
        cmd.push("--remote-control".into());
        cmd.push(def.name.clone());
    }
    tmux::new_session(&def.name, &dir, &cmd)?;
    let mut live = st.live.lock().unwrap();
    let l = live.entry(def.name.clone()).or_default();
    l.last_start = Some(Instant::now());
    l.rc_pending = !logged_in;
    l.limited = false;
    l.bypass_answered = 0;
    Ok(())
}

/// One supervision pass: (re)start sessions, read screens, drive logins, push changes.
fn supervise(st: &State) {
    let running = tmux::list_sessions().unwrap_or_default();
    let running_names: HashSet<&str> = running.iter().map(|r| r.name.as_str()).collect();
    let defs = st.cfg.lock().unwrap().sessions.clone();

    for def in &defs {
        if running_names.contains(def.name.as_str()) || !def.autostart || def.stopped {
            continue;
        }
        let recently = st
            .live
            .lock()
            .unwrap()
            .get(&def.name)
            .and_then(|l| l.last_start)
            .is_some_and(|t| t.elapsed() < RESTART_BACKOFF);
        if !recently {
            if let Err(e) = start_session(st, def) {
                eprintln!("starting {}: {e:#}", def.name);
                st.live.lock().unwrap().entry(def.name.clone()).or_default().last_start = Some(Instant::now());
            }
        }
    }

    let mut view = Vec::new();
    for r in &running {
        let screen = tmux::capture(&r.name, HISTORY).unwrap_or_default();
        let counts = Counts::of(&screen);
        let def = defs.iter().find(|d| d.name == r.name);

        let mut live = st.live.lock().unwrap();
        let l = live.entry(r.name.clone()).or_default();
        l.limit_base = l.limit_base.min(counts.limit); // screen cleared / scrolled away
        l.limited = counts.limit > l.limit_base;
        l.needs_login = NEEDS_LOGIN.is_match(&last_lines(&screen, 8))
            || def.is_some_and(|d| d.isolated && !has_credentials(&session_config_dir(st, d)));
        // The user asked for skip-permissions, so accept its confirmation screen
        // (its default answer is "No, exit").
        let bypass = screen.lines().filter(|ln| BYPASS.is_match(ln)).count();
        l.bypass_answered = l.bypass_answered.min(bypass);
        if def.is_some_and(|d| d.skip_permissions) && bypass > l.bypass_answered {
            l.bypass_answered = bypass;
            let _ = tmux::key(&r.name, "Down");
            std::thread::sleep(Duration::from_millis(150));
            let _ = tmux::enter(&r.name);
        }
        drive_login(st, &r.name, l, &screen, counts, def);

        view.push(SessionInfo {
            id: r.name.clone(),
            name: r.name.clone(),
            account: def.and_then(|d| d.account.clone()).unwrap_or_else(|| {
                if l.needs_login { "not logged in" } else { "unknown" }.into()
            }),
            cwd: def.map(|d| d.dir.clone()).unwrap_or_else(|| r.path.clone()),
            started_at: r.created,
            running: true,
            mode: SessionMode::Terminal,
            limited: l.limited,
            logging_in: l.login.is_some(),
            managed: def.is_some(),
            autostart: def.is_some_and(|d| d.autostart && !d.stopped),
            needs_login: l.needs_login,
            resume: def.is_some_and(|d| d.resume),
            skip_permissions: def.is_some_and(|d| d.skip_permissions),
        });
    }
    for def in defs.iter().filter(|d| !running_names.contains(d.name.as_str())) {
        view.push(SessionInfo {
            id: def.name.clone(),
            name: def.name.clone(),
            account: def.account.clone().unwrap_or_else(|| "unknown".into()),
            cwd: def.dir.clone(),
            started_at: 0,
            running: false,
            mode: SessionMode::Terminal,
            limited: false,
            logging_in: false,
            managed: true,
            autostart: def.autostart && !def.stopped,
            needs_login: false,
            resume: def.resume,
            skip_permissions: def.skip_permissions,
        });
    }
    view.sort_by(|a, b| a.name.cmp(&b.name));

    let changed = {
        let mut cur = st.view.lock().unwrap();
        let changed = serde_json::to_string(&*cur).ok() != serde_json::to_string(&view).ok();
        *cur = view.clone();
        changed
    };
    if changed {
        let _ = st.notify.send(DaemonMsg::Sessions { sessions: view });
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Advances an in-progress /login, or the settling period after one.
fn drive_login(st: &State, name: &str, l: &mut Live, screen: &str, c: Counts, def: Option<&SessionDef>) {
    let now = Instant::now();

    if let Some(login) = l.login.as_mut() {
        let b = login.base;
        if !login.url_sent {
            // Answer each new method menu / first-run prompt with Enter (the default choice).
            if c.method.saturating_sub(b.method) > login.method_enters {
                login.method_enters += 1;
                let _ = tmux::enter(name);
                return;
            }
            if c.prompts.saturating_sub(b.prompts) > login.prompt_enters {
                login.prompt_enters += 1;
                let _ = tmux::enter(name);
                return;
            }
            if let Some(url) = OAUTH_URL.find_iter(screen).map(|m| m.as_str()).find(|u| !login.baseline_urls.contains(*u)) {
                login.url_sent = true;
                let _ = st.notify.send(DaemonMsg::LoginUrl { session: name.into(), url: url.into() });
            }
        } else if !login.code_prompted && c.paste > b.paste {
            login.code_prompted = true;
            st.status(name, "waiting_code", None);
        }

        if login.url_sent && c.ok > b.ok {
            let account = login.account.clone();
            l.login = None;
            l.limit_base = c.limit; // old "limit reached" text no longer counts
            l.limited = false;
            l.settle_until = Some(now + Duration::from_secs(20));
            l.settle_base = c.prompts;
            l.settle_enters = 0;
            let _ = tmux::enter(name); // "Press Enter to continue"
            let _ = tmux::restore_size(name);
            if def.is_some() {
                st.update_def(name, |d| d.account = Some(account));
            }
            st.status(name, "success", None);
        } else if c.failed > b.failed {
            l.login = None;
            let _ = tmux::restore_size(name);
            st.status(name, "failed", Some("Claude Code reported a login error".into()));
        } else if login.started.elapsed() > LOGIN_TIMEOUT {
            l.login = None;
            let _ = tmux::key(name, "Escape");
            let _ = tmux::restore_size(name);
            st.status(name, "failed", Some("timed out".into()));
        }
        return;
    }

    if let Some(until) = l.settle_until {
        // settle_base already includes the "Press Enter to continue" answered on success;
        // each newer prompt (trust folder, theme, ...) gets one Enter.
        let new_prompts = c.prompts.saturating_sub(l.settle_base);
        if new_prompts > l.settle_enters {
            l.settle_enters += 1;
            l.last_settle_enter = Some(now);
            let _ = tmux::enter(name);
        } else if now > until || (l.rc_pending && l.last_settle_enter.is_none_or(|t| t.elapsed() > Duration::from_secs(4))) {
            l.settle_until = None;
            if l.rc_pending {
                // Logged in now: turn on Remote Control (it was started without it).
                l.rc_pending = false;
                let _ = tmux::type_line(name, "/remote-control");
            }
        }
    }
}

fn login_start(st: &State, name: &str, account: String) -> Result<()> {
    if !tmux::exists(name) {
        bail!("session '{name}' is not running");
    }
    let screen = tmux::capture(name, HISTORY).unwrap_or_default();
    let baseline_urls = OAUTH_URL.find_iter(&screen).map(|m| m.as_str().to_string()).collect();
    let base = Counts::of(&screen);
    // A session that has never logged in is sitting in first-run setup / the login screen.
    let fresh = st.def(name).is_some_and(|d| d.isolated && !has_credentials(&session_config_dir(st, &d)));
    let at_login_screen = fresh || LOGIN_METHOD.is_match(&last_lines(&screen, 8));
    {
        let mut live = st.live.lock().unwrap();
        let l = live.entry(name.to_string()).or_default();
        if l.login.is_some() {
            bail!("a login is already in progress for '{name}'");
        }
        l.login = Some(Login {
            account,
            started: Instant::now(),
            baseline_urls,
            // Never logged in: everything on screen (theme picker, login menu, …) is live
            // and needs answering. Otherwise only prompts that appear from now on count.
            base: if fresh {
                Counts { limit: base.limit, ..Counts::default() }
            } else if at_login_screen {
                Counts { method: base.method.saturating_sub(1), ..base }
            } else {
                base
            },
            method_enters: 0,
            prompt_enters: 0,
            url_sent: false,
            code_prompted: false,
        });
    }
    // Wide window so the sign-in URL isn't wrapped across lines.
    tmux::widen(name)?;
    if !at_login_screen {
        tmux::type_line(name, "/login")?;
    }
    Ok(())
}

fn login_cancel(st: &State, name: &str) {
    let had = st.live.lock().unwrap().get_mut(name).and_then(|l| l.login.take()).is_some();
    if had {
        let _ = tmux::key(name, "Escape");
        let _ = tmux::restore_size(name);
        st.status(name, "failed", Some("cancelled".into()));
    }
}

fn action(st: &State, name: &str, action: &str) -> Result<()> {
    match action {
        "start" => {
            st.update_def(name, |d| d.stopped = false);
            let def = st.def(name).context("not a managed session")?;
            if !tmux::exists(name) {
                start_session(st, &def)?;
            }
        }
        "stop" => {
            st.update_def(name, |d| d.stopped = true);
            login_cancel(st, name);
            tmux::kill(name)?;
        }
        "restart" => {
            let def = st.def(name).context("not a managed session")?;
            login_cancel(st, name);
            let _ = tmux::kill(name);
            st.update_def(name, |d| d.stopped = false);
            start_session(st, &def)?;
        }
        "manage" => {
            if st.def(name).is_some() {
                return Ok(());
            }
            let r = tmux::list_sessions()?.into_iter().find(|r| r.name == name).context("no such tmux session")?;
            // Keep its current config (whatever it was started with); it keeps running as-is.
            st.cfg.lock().unwrap().sessions.push(SessionDef {
                name: name.into(),
                dir: r.path,
                account: None,
                autostart: true,
                stopped: false,
                isolated: false,
                resume: true,
                skip_permissions: false,
                extra_args: vec![],
            });
            st.save();
        }
        "unmanage" => {
            st.cfg.lock().unwrap().sessions.retain(|d| d.name != name);
            st.save();
        }
        "autostart_on" => {
            st.update_def(name, |d| {
                d.autostart = true;
                d.stopped = false;
            });
        }
        "autostart_off" => {
            st.update_def(name, |d| d.autostart = false);
        }
        "remote_control" => tmux::type_line(name, "/remote-control")?,
        "resume_on" | "resume_off" => {
            st.update_def(name, |d| d.resume = action == "resume_on");
        }
        "skip_on" | "skip_off" => {
            st.update_def(name, |d| d.skip_permissions = action == "skip_on");
        }
        other => bail!("unknown action '{other}'"),
    }
    Ok(())
}

fn create_session(
    st: &State,
    name: Option<String>,
    cwd: String,
    resume: bool,
    skip_permissions: bool,
) -> Result<SessionInfo> {
    let dir = expand(&st.home, &cwd);
    if !dir.is_dir() {
        bail!("directory does not exist: {}", dir.display());
    }
    let name = name
        .filter(|n| !n.trim().is_empty())
        .or_else(|| dir.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default()
        .trim()
        .to_string();
    if !valid_name(&name) {
        bail!("session names may only use letters, digits, - and _ ('{name}')");
    }
    if st.def(&name).is_some() || tmux::exists(&name) {
        bail!("a session named '{name}' already exists");
    }
    let def = SessionDef {
        name: name.clone(),
        dir: cwd.trim().to_string(),
        account: None,
        autostart: true,
        stopped: false,
        isolated: true,
        resume,
        skip_permissions,
        extra_args: vec![],
    };
    st.cfg.lock().unwrap().sessions.push(def.clone());
    st.save();
    start_session(st, &def)?;
    Ok(SessionInfo {
        id: name.clone(),
        name,
        account: "not logged in".into(),
        cwd: def.dir,
        started_at: 0,
        running: true,
        mode: SessionMode::Terminal,
        limited: false,
        logging_in: false,
        managed: true,
        autostart: true,
        needs_login: true,
        resume,
        skip_permissions,
    })
}

// ---- file preview (kept for a future UI) ------------------------------------------------

fn read_file(st: &State, path: &str) -> Result<(PathBuf, Vec<u8>)> {
    let p = expand(&st.home, path);
    let meta = std::fs::metadata(&p).with_context(|| format!("{}", p.display()))?;
    if !meta.is_file() || meta.len() > proto::MAX_FILE_BYTES {
        bail!("not a previewable file: {}", p.display());
    }
    Ok((p.clone(), std::fs::read(&p)?))
}

// ---- websocket -----------------------------------------------------------------

async fn handle(stream: TcpStream, st: Arc<State>) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<DaemonMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(Message::text(serde_json::to_string(&m).expect("serializable"))).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut notify_task = None;
    while let Some(frame) = source.next().await {
        let text = match frame? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let msg: ClientMsg = match serde_json::from_str(text.as_str()) {
            Ok(m) => m,
            Err(e) => {
                let _ = out.send(DaemonMsg::Error { message: format!("bad message: {e}") });
                continue;
            }
        };
        if notify_task.is_none() {
            match msg {
                ClientMsg::Hello { token, protocol } if token == st.args.token => {
                    if protocol != proto::PROTOCOL_VERSION {
                        let _ = out.send(DaemonMsg::Error {
                            message: format!(
                                "protocol mismatch: app v{protocol}, daemon v{} (update both)",
                                proto::PROTOCOL_VERSION
                            ),
                        });
                        break;
                    }
                    let _ = out.send(DaemonMsg::Welcome { host: st.host.clone(), protocol: proto::PROTOCOL_VERSION });
                    let mut rx = st.notify.subscribe();
                    let out2 = out.clone();
                    notify_task = Some(tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(m) => {
                                    if out2.send(m).is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(_) => break,
                            }
                        }
                    }));
                    continue;
                }
                _ => {
                    let _ = out.send(DaemonMsg::Error { message: "authentication failed".into() });
                    break;
                }
            }
        }

        // tmux calls block; keep them off the async runtime.
        let st2 = st.clone();
        let out2 = out.clone();
        tokio::task::spawn_blocking(move || {
            let r: Result<()> = (|| {
                match msg {
                    ClientMsg::Hello { .. } => {}
                    ClientMsg::ListSessions => {
                        let _ = out2.send(DaemonMsg::Sessions { sessions: st2.view.lock().unwrap().clone() });
                    }
                    ClientMsg::ListAccounts => {
                        let _ = out2.send(DaemonMsg::Accounts { accounts: vec![] });
                    }
                    ClientMsg::StartSession { cwd, name, resume, skip_permissions, .. } => {
                        let info =
                            create_session(&st2, name, cwd, resume.unwrap_or(true), skip_permissions.unwrap_or(false))?;
                        let _ = out2.send(DaemonMsg::SessionStarted { session: info });
                    }
                    ClientMsg::SessionAction { session, action: a } => action(&st2, &session, &a)?,
                    ClientMsg::LoginStart { session, account } => login_start(&st2, &session, account)?,
                    ClientMsg::LoginCode { session, code } => {
                        let active = st2.live.lock().unwrap().get(&session).is_some_and(|l| l.login.is_some());
                        if !active {
                            bail!("no login in progress for '{session}'");
                        }
                        tmux::type_line(&session, code.trim())?;
                    }
                    ClientMsg::LoginCancel { session } => login_cancel(&st2, &session),
                    ClientMsg::Command { session, line } => tmux::type_line(&session, &line)?,
                    ClientMsg::Kill { session } => action(&st2, &session, "stop")?,
                    ClientMsg::ReadFile { request, path, .. } => {
                        let _ = out2.send(match read_file(&st2, &path) {
                            Ok((p, data)) => DaemonMsg::File {
                                request,
                                path: p.display().to_string(),
                                mime: "application/octet-stream".into(),
                                data: proto::encode(&data),
                            },
                            Err(e) => DaemonMsg::FileError { request, message: format!("{e:#}") },
                        });
                    }
                    ClientMsg::Attach { .. }
                    | ClientMsg::Detach { .. }
                    | ClientMsg::Input { .. }
                    | ClientMsg::Resize { .. }
                    | ClientMsg::SendChat { .. } => bail!("not supported by this daemon (use tmux attach)"),
                }
                Ok(())
            })();
            if let Err(e) = r {
                let _ = out2.send(DaemonMsg::Error { message: format!("{e:#}") });
            }
        });
    }

    if let Some(t) = notify_task {
        t.abort();
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}
