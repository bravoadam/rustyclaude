//! Thin wrapper over the tmux CLI.

use std::{path::Path, process::Command, sync::OnceLock, thread::sleep, time::Duration};

use anyhow::{bail, Result};

static TMUX: OnceLock<String> = OnceLock::new();

pub fn init(path: &str) {
    let _ = TMUX.set(path.to_string());
}

fn run(args: &[&str]) -> Result<String> {
    let out = Command::new(TMUX.get().map(String::as_str).unwrap_or("tmux")).args(args).output()?;
    if !out.status.success() {
        bail!("tmux {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Exact-match target for a session's active pane (avoids tmux prefix matching).
fn target(name: &str) -> String {
    format!("={name}:")
}

pub fn version() -> Result<String> {
    run(&["-V"])
}

pub struct Running {
    pub name: String,
    pub created: u64,
    pub path: String,
}

pub fn list_sessions() -> Result<Vec<Running>> {
    let out = match run(&["list-sessions", "-F", "#{session_name}\t#{session_created}\t#{pane_current_path}"]) {
        Ok(o) => o,
        // No server running = no sessions.
        Err(e) if e.to_string().contains("no server") || e.to_string().contains("No such file") => return Ok(vec![]),
        Err(e) if e.to_string().contains("error connecting") => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some(Running {
                name: it.next()?.to_string(),
                created: it.next()?.parse().unwrap_or(0),
                path: it.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

pub fn exists(name: &str) -> bool {
    run(&["has-session", "-t", &format!("={name}")]).is_ok()
}

pub fn new_session(name: &str, dir: &Path, cmd: &[String]) -> Result<()> {
    let dir = dir.to_string_lossy();
    let mut args = vec!["new-session", "-d", "-s", name, "-c", &dir, "-x", "200", "-y", "50"];
    args.extend(cmd.iter().map(String::as_str));
    run(&args)?;
    Ok(())
}

pub fn kill(name: &str) -> Result<()> {
    run(&["kill-session", "-t", &format!("={name}")])?;
    Ok(())
}

/// Plain text of the pane, wrapped lines joined, including `history` lines of scrollback.
pub fn capture(name: &str, history: u32) -> Result<String> {
    run(&["capture-pane", "-p", "-J", "-t", &target(name), "-S", &format!("-{history}")])
}

pub fn key(name: &str, key: &str) -> Result<()> {
    run(&["send-keys", "-t", &target(name), key])?;
    Ok(())
}

pub fn enter(name: &str) -> Result<()> {
    key(name, "Enter")
}

/// Clears the prompt, types `line` literally, then presses Enter separately so the
/// TUI treats it as typing rather than a paste.
pub fn type_line(name: &str, line: &str) -> Result<()> {
    key(name, "C-u")?;
    sleep(Duration::from_millis(120));
    run(&["send-keys", "-t", &target(name), "-l", line])?;
    sleep(Duration::from_millis(250));
    enter(name)
}

/// Very wide window so long URLs print on one line.
pub fn widen(name: &str) -> Result<()> {
    run(&["resize-window", "-t", &target(name), "-x", "1000"])?;
    Ok(())
}

/// Back to following the attached client's size.
pub fn restore_size(name: &str) -> Result<()> {
    let _ = run(&["set-option", "-t", &format!("={name}"), "window-size", "latest"]);
    let _ = run(&["resize-window", "-A", "-t", &target(name)]);
    Ok(())
}
