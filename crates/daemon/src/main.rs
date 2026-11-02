//! rustyclaude-daemon: runs on each host, owns Claude Code sessions, and exposes
//! them to RustyClaude clients over a WebSocket.
//!
//! Two session modes:
//! - terminal: interactive `claude` in a PTY (usable from the official apps via Remote Control)
//! - chat:     `claude -p --input-format stream-json --output-format stream-json`, giving
//!             structured events for a rich UI (attachments, inline media, tool cards)
//!
//! Accounts: every `~/.claude-<name>` directory is an account (used as
//! CLAUDE_CONFIG_DIR), plus `default` which uses the normal `~/.claude`.
//!
//! Sessions live as long as the daemon process does.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use rustyclaude_protocol::{
    self as proto, Attachment, ClientMsg, DaemonMsg, SessionInfo, SessionMode, DEFAULT_ACCOUNT,
};
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::Message;

/// Strips ANSI/OSC escape sequences so terminal output can be pattern-matched.
static ANSI: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
});
static LIMIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)usage limit|limit reached|limit will reset|out of extra usage|reached your [a-z ]*limit").unwrap()
});
static OAUTH_URL: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"https://[^\s\x07\x1b]*oauth[^\s\x07\x1b]*").unwrap());
static LOGIN_FAILED: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)oauth error|login failed|invalid (authorization )?code").unwrap());

/// PTY width while /login runs, so the sign-in URL is printed on one line.
const LOGIN_COLS: u16 = 1000;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

const SCROLLBACK_BYTES: usize = 512 * 1024;
const CHAT_HISTORY_EVENTS: usize = 5000;
const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
const PERMISSION_MODES: &[&str] = &["default", "acceptEdits", "plan", "bypassPermissions"];

#[derive(Parser)]
struct Args {
    /// Address to listen on. Bind to your Tailscale IP to reach it from other machines.
    #[arg(long, default_value = "127.0.0.1:7878")]
    bind: String,
    /// Shared secret clients must present.
    #[arg(long, env = "RUSTYCLAUDE_TOKEN")]
    token: String,
    /// Claude Code executable.
    #[arg(long, default_value = "claude")]
    cmd: String,
    /// Extra arguments for terminal-mode sessions.
    #[arg(long, allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
    /// Don't start terminal sessions with --remote-control.
    #[arg(long)]
    no_remote_control: bool,
}

struct Login {
    account: String,
    started: Instant,
    text: String,
    entered: bool,
    url_sent: bool,
    code_prompted: bool,
}

#[derive(Clone, Debug)]
enum Event {
    Data(Vec<u8>),
    Chat(Value),
    Exit(Option<u32>),
}

enum Backend {
    Pty {
        writer: Mutex<Box<dyn Write + Send>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    },
    Chat {
        stdin: Mutex<Option<ChildStdin>>,
        child: Arc<Mutex<Child>>,
    },
}

/// Replay buffer for late attachers. Its lock also orders snapshot-vs-append so
/// an attach never misses or duplicates output.
enum History {
    Bytes(Vec<u8>),
    Events(Vec<Value>),
}

struct Session {
    info: Mutex<SessionInfo>,
    login: Mutex<Option<Login>>,
    /// Last size a client asked for (restored after /login widens the PTY).
    size: Mutex<(u16, u16)>,
    /// Ignore "usage limit" text until then (old messages get redrawn after a switch).
    limit_quiet_until: Mutex<Instant>,
    backend: Backend,
    history: Mutex<History>,
    tx: broadcast::Sender<Event>,
}

impl Session {
    fn publish(&self, ev: Event) {
        let mut h = self.history.lock().unwrap();
        match (&mut *h, &ev) {
            (History::Bytes(b), Event::Data(d)) => {
                b.extend_from_slice(d);
                if b.len() > SCROLLBACK_BYTES {
                    let excess = b.len() - SCROLLBACK_BYTES;
                    b.drain(..excess);
                }
            }
            (History::Events(v), Event::Chat(e)) => {
                v.push(e.clone());
                if v.len() > CHAT_HISTORY_EVENTS {
                    let excess = v.len() - CHAT_HISTORY_EVENTS;
                    v.drain(..excess);
                }
            }
            (_, Event::Exit(_)) => self.info.lock().unwrap().running = false,
            _ => {}
        }
        let _ = self.tx.send(ev);
    }
}

struct State {
    args: Args,
    host: String,
    home: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next_id: Mutex<u64>,
    /// Pushed to every connected client (login progress, session list changes).
    notify: broadcast::Sender<DaemonMsg>,
}

impl State {
    fn push_sessions(&self) {
        let _ = self.notify.send(DaemonMsg::Sessions { sessions: list(self) });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME not set")?;
    let listener = TcpListener::bind(&args.bind).await?;
    eprintln!("rustyclaude-daemon listening on ws://{}", args.bind);
    let state = Arc::new(State {
        host: gethostname::gethostname().to_string_lossy().into_owned(),
        args,
        home,
        sessions: Mutex::new(HashMap::new()),
        next_id: Mutex::new(1),
        notify: broadcast::channel(256).0,
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

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn accounts(st: &State) -> Vec<String> {
    let mut out = vec![DEFAULT_ACCOUNT.to_string()];
    if let Ok(rd) = std::fs::read_dir(&st.home) {
        let mut found: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str()?.strip_prefix(".claude-").map(str::to_string))
            .filter(|n| !n.is_empty())
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

fn list(st: &State) -> Vec<SessionInfo> {
    let mut v: Vec<SessionInfo> =
        st.sessions.lock().unwrap().values().map(|s| s.info.lock().unwrap().clone()).collect();
    v.sort_by_key(|s| s.started_at);
    v
}

fn expand(home: &Path, p: &str) -> PathBuf {
    match p.trim() {
        "" | "~" => home.to_path_buf(),
        c if c.starts_with("~/") => home.join(&c[2..]),
        c => PathBuf::from(c),
    }
}

fn config_dir_env(st: &State, account: &str) -> Option<PathBuf> {
    (account != DEFAULT_ACCOUNT).then(|| st.home.join(format!(".claude-{account}")))
}

fn start(
    st: &Arc<State>,
    account: String,
    cwd: String,
    name: Option<String>,
    mode: SessionMode,
    permission_mode: Option<String>,
) -> Result<SessionInfo> {
    // Only accept accounts we enumerated, so the name can't smuggle a path.
    if !accounts(st).contains(&account) {
        bail!("unknown account '{account}'");
    }
    let cwd = expand(&st.home, &cwd);
    if !cwd.is_dir() {
        bail!("directory does not exist: {}", cwd.display());
    }
    let id = {
        let mut n = st.next_id.lock().unwrap();
        *n += 1;
        format!("s{}-{}", now(), *n - 1)
    };
    let info = SessionInfo {
        id: id.clone(),
        name: name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
            cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "session".into())
        }),
        account: account.clone(),
        cwd: cwd.display().to_string(),
        started_at: now(),
        running: true,
        mode,
        limited: false,
        logging_in: false,
    };
    let sess = match mode {
        SessionMode::Terminal => start_pty(st, &account, &cwd, info.clone())?,
        SessionMode::Chat => start_chat(st, &account, &cwd, permission_mode, info.clone())?,
    };
    st.sessions.lock().unwrap().insert(id, sess);
    Ok(info)
}

fn new_session(info: SessionInfo, backend: Backend, history: History) -> Arc<Session> {
    Arc::new(Session {
        info: Mutex::new(info),
        login: Mutex::new(None),
        size: Mutex::new((120, 40)),
        limit_quiet_until: Mutex::new(Instant::now()),
        backend,
        history: Mutex::new(history),
        tx: broadcast::channel(1024).0,
    })
}

fn start_pty(st: &Arc<State>, account: &str, cwd: &Path, info: SessionInfo) -> Result<Arc<Session>> {
    let pty = native_pty_system().openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })?;
    let mut cmd = CommandBuilder::new(&st.args.cmd);
    cmd.args(&st.args.args);
    if !st.args.no_remote_control {
        cmd.arg("--remote-control");
        cmd.arg(&info.name);
    }
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    if let Some(dir) = config_dir_env(st, account) {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = pty.slave.spawn_command(cmd).with_context(|| format!("spawning '{}'", st.args.cmd))?;
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader()?;
    let sess = new_session(
        info,
        Backend::Pty {
            writer: Mutex::new(pty.master.take_writer()?),
            killer: Mutex::new(child.clone_killer()),
            master: Mutex::new(pty.master),
        },
        History::Bytes(Vec::new()),
    );
    let s = sess.clone();
    let st = st.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    observe(&st, &s, &buf[..n]);
                    s.publish(Event::Data(buf[..n].to_vec()));
                }
            }
        }
        let code = child.wait().ok().map(|st| st.exit_code());
        s.publish(Event::Exit(code));
        if s.login.lock().unwrap().take().is_some() {
            finish_login(&st, &s, false, Some("session exited".into()));
        }
        st.push_sessions();
    });
    Ok(sess)
}

fn start_chat(
    st: &State,
    account: &str,
    cwd: &Path,
    permission_mode: Option<String>,
    info: SessionInfo,
) -> Result<Arc<Session>> {
    let mut cmd = Command::new(&st.args.cmd);
    cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"]);
    if let Some(pm) = permission_mode.filter(|p| p != "default") {
        if !PERMISSION_MODES.contains(&pm.as_str()) {
            bail!("unknown permission mode '{pm}'");
        }
        cmd.args(["--permission-mode", &pm]);
    }
    cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = config_dir_env(st, account) {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawning '{}'", st.args.cmd))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let child = Arc::new(Mutex::new(child));

    let sess = new_session(
        info,
        Backend::Chat { stdin: Mutex::new(stdin), child: child.clone() },
        History::Events(Vec::new()),
    );

    let s = sess.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            s.publish(Event::Chat(json!({ "type": "stderr", "text": line })));
        }
    });
    let s = sess.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let ev = serde_json::from_str(&line).unwrap_or_else(|_| json!({ "type": "stderr", "text": line }));
            s.publish(Event::Chat(ev));
        }
        // stdout closed: reap the process without holding the lock across a blocking wait.
        let code = loop {
            match child.lock().unwrap().try_wait() {
                Ok(Some(status)) => break status.code().map(|c| c as u32),
                Ok(None) => {}
                Err(_) => break None,
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        s.publish(Event::Exit(code));
    });
    Ok(sess)
}

fn safe_file_name(name: &str) -> String {
    let base = Path::new(name).file_name().and_then(|f| f.to_str()).unwrap_or("file");
    let cleaned: String =
        base.chars().map(|c| if c.is_alphanumeric() || "._- ".contains(c) { c } else { '_' }).collect();
    if cleaned.trim_matches('.').is_empty() { "file".into() } else { cleaned }
}

/// Builds the stream-json user message. Images go inline as image blocks (Claude sees
/// them directly); everything else is saved under `<cwd>/.rustyclaude/uploads/` and
/// referenced by path so Claude Code can read it with its own tools.
fn chat_payload(cwd: &Path, text: &str, attachments: &[Attachment]) -> Result<(Value, Vec<Value>)> {
    let mut content = Vec::new();
    let mut echoed = Vec::new();
    let mut saved = Vec::new();
    for a in attachments {
        if IMAGE_MIMES.contains(&a.mime.as_str()) {
            content.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": a.mime, "data": a.data },
            }));
            echoed.push(json!({ "name": a.name, "mime": a.mime, "data": a.data }));
        } else {
            let dir = cwd.join(".rustyclaude").join("uploads");
            std::fs::create_dir_all(&dir)?;
            let mut path = dir.join(safe_file_name(&a.name));
            let mut i = 1;
            while path.exists() {
                path = dir.join(format!("{i}-{}", safe_file_name(&a.name)));
                i += 1;
            }
            std::fs::write(&path, proto::decode(&a.data)?)?;
            let rel = path.strip_prefix(cwd).unwrap_or(&path).display().to_string();
            saved.push(rel.clone());
            echoed.push(json!({ "name": a.name, "mime": a.mime, "path": rel }));
        }
    }
    let mut full = text.to_string();
    if !saved.is_empty() {
        full.push_str("\n\n[Attached files, saved in the working directory:]\n");
        for p in &saved {
            full.push_str(&format!("- {p}\n"));
        }
    }
    if !full.trim().is_empty() {
        content.push(json!({ "type": "text", "text": full }));
    }
    let msg = json!({ "type": "user", "message": { "role": "user", "content": content } });
    Ok((msg, echoed))
}

fn send_chat(sess: &Session, text: String, attachments: Vec<Attachment>) -> Result<()> {
    let Backend::Chat { stdin, .. } = &sess.backend else { bail!("not a chat session") };
    if !sess.info.lock().unwrap().running {
        bail!("session has exited");
    }
    let cwd = PathBuf::from(&sess.info.lock().unwrap().cwd);
    let (msg, echoed) = chat_payload(&cwd, &text, &attachments)?;
    sess.publish(Event::Chat(json!({ "type": "local_user", "text": text, "attachments": echoed, "ts": now() })));
    let mut guard = stdin.lock().unwrap();
    let w = guard.as_mut().context("stdin closed")?;
    writeln!(w, "{}", serde_json::to_string(&msg)?)?;
    w.flush()?;
    Ok(())
}

fn mime_for(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        _ => "text/plain",
    }
}

fn read_file(st: &State, session: Option<&str>, path: &str) -> Result<(PathBuf, &'static str, Vec<u8>)> {
    let base = match session {
        Some(id) => PathBuf::from(&get(st, id)?.info.lock().unwrap().cwd),
        None => st.home.clone(),
    };
    let p = expand(&st.home, path);
    let p = if p.is_absolute() { p } else { base.join(p) };
    let meta = std::fs::metadata(&p).with_context(|| format!("{}", p.display()))?;
    if !meta.is_file() {
        bail!("not a file: {}", p.display());
    }
    if meta.len() > proto::MAX_FILE_BYTES {
        bail!("file too large to preview ({} MB)", meta.len() / 1_000_000);
    }
    let data = std::fs::read(&p)?;
    Ok((p.clone(), mime_for(&p), data))
}

fn get(st: &State, id: &str) -> Result<Arc<Session>> {
    st.sessions.lock().unwrap().get(id).cloned().with_context(|| format!("no session '{id}'"))
}

fn set_pty_size(sess: &Session, cols: u16, rows: u16) {
    if let Backend::Pty { master, .. } = &sess.backend {
        let _ = master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }
}

fn resize(sess: &Session, cols: u16, rows: u16) {
    if cols == 0 || rows == 0 {
        return;
    }
    *sess.size.lock().unwrap() = (cols, rows);
    if sess.login.lock().unwrap().is_none() {
        set_pty_size(sess, cols, rows);
    }
}

fn write_pty(sess: &Session, bytes: &[u8]) -> Result<()> {
    let Backend::Pty { writer, .. } = &sess.backend else { bail!("not a terminal session") };
    let mut w = writer.lock().unwrap();
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

/// Types a line into the TUI: text first, Enter separately, so it isn't treated as a paste.
fn type_line(sess: Arc<Session>, line: String) {
    std::thread::spawn(move || {
        let _ = write_pty(&sess, b"\x15"); // ctrl-u: clear whatever is in the prompt
        std::thread::sleep(Duration::from_millis(120));
        let _ = write_pty(&sess, line.as_bytes());
        std::thread::sleep(Duration::from_millis(200));
        let _ = write_pty(&sess, b"\r");
    });
}

// ---- account switching (/login) and usage-limit detection ----------------------

fn observe(st: &State, sess: &Arc<Session>, bytes: &[u8]) {
    let raw = String::from_utf8_lossy(bytes);
    let text = ANSI.replace_all(&raw, "");

    if LIMIT.is_match(&text) && Instant::now() >= *sess.limit_quiet_until.lock().unwrap() {
        let changed = {
            let mut info = sess.info.lock().unwrap();
            !std::mem::replace(&mut info.limited, true)
        };
        if changed {
            st.push_sessions();
        }
    }

    let mut guard = sess.login.lock().unwrap();
    let Some(login) = guard.as_mut() else { return };
    login.text.push_str(&text);
    if login.text.len() > 256 * 1024 {
        let cut = login.text.len() - 128 * 1024;
        let cut = (cut..login.text.len()).find(|i| login.text.is_char_boundary(*i)).unwrap_or(0);
        login.text.drain(..cut);
    }
    let id = sess.info.lock().unwrap().id.clone();

    if !login.entered && !login.url_sent && login.text.to_lowercase().contains("login method") {
        login.entered = true; // default choice: Claude account with subscription
        let _ = write_pty(sess, b"\r");
    }
    if !login.url_sent {
        if let Some(m) = OAUTH_URL.find(&login.text) {
            login.url_sent = true;
            let _ = st.notify.send(DaemonMsg::LoginUrl { session: id.clone(), url: m.as_str().to_string() });
        }
    }
    if login.url_sent && !login.code_prompted && login.text.to_lowercase().contains("paste code") {
        login.code_prompted = true;
        let _ = st.notify.send(DaemonMsg::LoginStatus { session: id.clone(), status: "waiting_code".into(), message: None });
    }
    let lower = login.text.to_lowercase();
    if lower.contains("login successful") {
        let account = login.account.clone();
        *guard = None;
        drop(guard);
        sess.info.lock().unwrap().account = account;
        let s = sess.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = write_pty(&s, b"\r"); // "Press Enter to continue"
        });
        finish_login(st, sess, true, None);
    } else if LOGIN_FAILED.is_match(&login.text) {
        *guard = None;
        drop(guard);
        finish_login(st, sess, false, Some("Claude Code reported a login error".into()));
    }
}

/// Call after clearing `sess.login`.
fn finish_login(st: &State, sess: &Session, ok: bool, message: Option<String>) {
    let id = {
        let mut info = sess.info.lock().unwrap();
        info.logging_in = false;
        if ok {
            info.limited = false;
        }
        info.id.clone()
    };
    if ok {
        *sess.limit_quiet_until.lock().unwrap() = Instant::now() + Duration::from_secs(15);
    }
    let (cols, rows) = *sess.size.lock().unwrap();
    set_pty_size(sess, cols, rows);
    let status = if ok { "success" } else { "failed" };
    let _ = st.notify.send(DaemonMsg::LoginStatus { session: id, status: status.into(), message });
    st.push_sessions();
}

fn login_start(st: &Arc<State>, sess: &Arc<Session>, account: String) -> Result<()> {
    if !matches!(sess.backend, Backend::Pty { .. }) {
        bail!("only terminal sessions can switch accounts");
    }
    {
        let mut info = sess.info.lock().unwrap();
        if !info.running {
            bail!("session has exited");
        }
        info.logging_in = true;
    }
    let started = Instant::now();
    *sess.login.lock().unwrap() =
        Some(Login { account, started, text: String::new(), entered: false, url_sent: false, code_prompted: false });
    let rows = sess.size.lock().unwrap().1;
    set_pty_size(sess, LOGIN_COLS, rows);
    st.push_sessions();
    type_line(sess.clone(), "/login".into());

    // If no method menu showed up, press Enter once; give up after the timeout.
    let (s, st2) = (sess.clone(), st.clone());
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3000));
        {
            let mut g = s.login.lock().unwrap();
            if let Some(l) = g.as_mut().filter(|l| l.started == started && !l.entered && !l.url_sent) {
                l.entered = true;
                let _ = write_pty(&s, b"\r");
            }
        }
        std::thread::sleep(LOGIN_TIMEOUT);
        let timed_out = {
            let mut g = s.login.lock().unwrap();
            if g.as_ref().is_some_and(|l| l.started == started) { g.take().is_some() } else { false }
        };
        if timed_out {
            let _ = write_pty(&s, b"\x1b");
            finish_login(&st2, &s, false, Some("timed out".into()));
        }
    });
    Ok(())
}

fn kill(sess: &Session) -> Result<()> {
    match &sess.backend {
        Backend::Pty { killer, .. } => killer.lock().unwrap().kill()?,
        Backend::Chat { stdin, child } => {
            stdin.lock().unwrap().take(); // EOF lets claude exit cleanly...
            let _ = child.lock().unwrap().kill(); // ...but don't wait for it.
        }
    }
    Ok(())
}

fn to_msg(id: &str, ev: Event) -> DaemonMsg {
    match ev {
        Event::Data(d) => DaemonMsg::Output { session: id.into(), data: proto::encode(&d) },
        Event::Chat(event) => DaemonMsg::Chat { session: id.into(), event },
        Event::Exit(code) => DaemonMsg::Exited { session: id.into(), code },
    }
}

fn attach(sess: Arc<Session>, out: mpsc::UnboundedSender<DaemonMsg>) -> JoinHandle<()> {
    let id = sess.info.lock().unwrap().id.clone();
    let (replay, mut rx, running) = {
        let h = sess.history.lock().unwrap();
        let replay: Vec<DaemonMsg> = match &*h {
            History::Bytes(b) => vec![to_msg(&id, Event::Data(b.clone()))],
            History::Events(v) => v.iter().map(|e| to_msg(&id, Event::Chat(e.clone()))).collect(),
        };
        (replay, sess.tx.subscribe(), sess.info.lock().unwrap().running)
    };
    tokio::spawn(async move {
        for m in replay {
            let _ = out.send(m);
        }
        if !running {
            let _ = out.send(DaemonMsg::Exited { session: id, code: None });
            return;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let done = matches!(ev, Event::Exit(_));
                    if out.send(to_msg(&id, ev)).is_err() || done {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

async fn handle(stream: TcpStream, st: Arc<State>) -> Result<()> {
    let ws = tokio_tungstenite::accept_async_with_config(
        stream,
        Some(
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(128 << 20))
                .max_frame_size(Some(128 << 20)),
        ),
    )
    .await?;
    let (mut sink, mut source) = ws.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<DaemonMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            let json = serde_json::to_string(&m).expect("serializable");
            if sink.send(Message::text(json)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut authed = false;
    let mut attached: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut notify_task: Option<JoinHandle<()>> = None;

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
        if !authed {
            match msg {
                ClientMsg::Hello { token, protocol } if token == st.args.token => {
                    if protocol != proto::PROTOCOL_VERSION {
                        let _ = out.send(DaemonMsg::Error {
                            message: format!(
                                "protocol mismatch: client v{protocol}, daemon v{} (update both)",
                                proto::PROTOCOL_VERSION
                            ),
                        });
                        break;
                    }
                    authed = true;
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
        if let ClientMsg::ReadFile { request, session, path } = msg {
            // Off the socket loop: files can be large.
            let st = st.clone();
            let out = out.clone();
            tokio::task::spawn_blocking(move || {
                let _ = out.send(match read_file(&st, session.as_deref(), &path) {
                    Ok((p, mime, data)) => DaemonMsg::File {
                        request,
                        path: p.display().to_string(),
                        mime: mime.into(),
                        data: proto::encode(&data),
                    },
                    Err(e) => DaemonMsg::FileError { request, message: format!("{e:#}") },
                });
            });
            continue;
        }
        let result: Result<()> = (|| {
            match msg {
                ClientMsg::Hello { .. } | ClientMsg::ReadFile { .. } => {}
                ClientMsg::ListSessions => {
                    let _ = out.send(DaemonMsg::Sessions { sessions: list(&st) });
                }
                ClientMsg::ListAccounts => {
                    let _ = out.send(DaemonMsg::Accounts { accounts: accounts(&st) });
                }
                ClientMsg::StartSession { account, cwd, name, mode, permission_mode } => {
                    let info = start(&st, account, cwd, name, mode, permission_mode)?;
                    let _ = out.send(DaemonMsg::SessionStarted { session: info });
                    st.push_sessions();
                }
                ClientMsg::LoginStart { session, account } => login_start(&st, &get(&st, &session)?, account)?,
                ClientMsg::LoginCode { session, code } => {
                    let sess = get(&st, &session)?;
                    if sess.login.lock().unwrap().is_none() {
                        bail!("no account switch in progress");
                    }
                    type_line(sess, code.trim().to_string());
                }
                ClientMsg::LoginCancel { session } => {
                    let sess = get(&st, &session)?;
                    if sess.login.lock().unwrap().take().is_some() {
                        write_pty(&sess, b"\x1b")?;
                        finish_login(&st, &sess, false, Some("cancelled".into()));
                    }
                }
                ClientMsg::Command { session, line } => type_line(get(&st, &session)?, line),
                ClientMsg::SendChat { session, text, attachments } => {
                    send_chat(&*get(&st, &session)?, text, attachments)?;
                }
                ClientMsg::Attach { session, cols, rows } => {
                    let sess = get(&st, &session)?;
                    resize(&sess, cols, rows);
                    if let Some(old) = attached.insert(session, attach(sess, out.clone())) {
                        old.abort();
                    }
                }
                ClientMsg::Detach { session } => {
                    if let Some(h) = attached.remove(&session) {
                        h.abort();
                    }
                }
                ClientMsg::Input { session, data } => write_pty(&*get(&st, &session)?, &proto::decode(&data)?)?,
                ClientMsg::Resize { session, cols, rows } => resize(&*get(&st, &session)?, cols, rows),
                ClientMsg::Kill { session } => {
                    let sess = get(&st, &session)?;
                    if sess.info.lock().unwrap().running {
                        kill(&sess)?;
                    } else {
                        // Already exited: kill means "remove from the list".
                        st.sessions.lock().unwrap().remove(&session);
                        if let Some(h) = attached.remove(&session) {
                            h.abort();
                        }
                        st.push_sessions();
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = out.send(DaemonMsg::Error { message: format!("{e:#}") });
        }
    }

    for (_, h) in attached {
        h.abort();
    }
    if let Some(t) = notify_task {
        t.abort();
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}
