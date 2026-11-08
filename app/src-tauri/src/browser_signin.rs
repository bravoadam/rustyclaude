//! Linux only: sign in to claude.ai through a real browser. claude.ai's bot check rejects
//! WebKitGTK for email and Apple sign-in, so we open claude.ai/login in Brave (or Chromium)
//! with a throwaway profile, wait for the session cookie over the DevTools protocol, copy
//! the claude.ai cookies into the account's webview and throw the profile away.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

const BROWSERS: &[&str] = &["brave-browser", "brave", "chromium", "chromium-browser", "google-chrome", "google-chrome-stable"];
const TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A claude.ai cookie as Chromium reports it.
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Seconds since the epoch; None for session cookies.
    pub expires: Option<i64>,
}

fn find_browser() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    BROWSERS.iter().find_map(|b| std::env::split_paths(&path).map(|d| d.join(b)).find(|p| p.is_file()))
}

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Minimal HTTP GET against the local DevTools endpoint.
fn http_get(port: u16, path: &str) -> Option<Value> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    // DevTools keeps the connection open, so read until Content-Length bytes arrived.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok());
            if len.is_some_and(|len| body.len() >= len) {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf);
    serde_json::from_str(text.split_once("\r\n\r\n")?.1).ok()
}

struct Session {
    child: Child,
    profile: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        // brave-browser is a wrapper script, so killing the child may leave the browser
        // running; stop whatever uses this throwaway profile too.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("pkill").args(["-f", "--"]).arg(format!("--user-data-dir={}", self.profile.display())).status();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn cdp<S: Read + Write>(ws: &mut tokio_tungstenite::tungstenite::WebSocket<S>, id: u64, method: &str) -> Option<Value> {
    ws.send(tokio_tungstenite::tungstenite::Message::text(json!({ "id": id, "method": method }).to_string())).ok()?;
    loop {
        let msg = ws.read().ok()?;
        let Ok(text) = msg.to_text() else { continue };
        let v: Value = serde_json::from_str(text).ok()?;
        if v["id"] == id {
            return Some(v["result"].clone());
        }
    }
}

/// Opens the browser and blocks until the user is signed in, the window is closed, or it
/// times out. Returns the claude.ai cookies.
pub fn run(profile_root: &Path) -> Result<Vec<BrowserCookie>, String> {
    let browser = find_browser().ok_or("Install Brave or Chromium to sign in with email.")?;
    let port = free_port().map_err(|e| e.to_string())?;
    let profile = profile_root.join(format!("signin-{port}"));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let child = Command::new(&browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-allow-origins=http://127.0.0.1")
        .args(["--no-first-run", "--no-default-browser-check", "--new-window", "https://claude.ai/login"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", browser.display()))?;
    let mut session = Session { child, profile };

    let start = Instant::now();
    let ws_url = loop {
        if let Some(v) = http_get(port, "/json/version") {
            if let Some(u) = v["webSocketDebuggerUrl"].as_str() {
                break u.to_string();
            }
        }
        if start.elapsed() > Duration::from_secs(20) {
            return Err("the browser didn't start".into());
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let (mut ws, _) = tokio_tungstenite::tungstenite::connect(ws_url.as_str()).map_err(|e| format!("DevTools: {e}"))?;

    let mut id = 0;
    let mut signed_in_at: Option<Instant> = None;
    loop {
        if start.elapsed() > TIMEOUT {
            return Err("sign-in timed out".into());
        }
        if let Ok(Some(_)) = session.child.try_wait() {
            return Err("the sign-in window was closed".into());
        }
        id += 1;
        let r = cdp(&mut ws, id, "Storage.getCookies").ok_or("the sign-in window was closed")?;
        let cookies: Vec<BrowserCookie> = r["cookies"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter(|c| {
                let d = c["domain"].as_str().unwrap_or("").trim_start_matches('.');
                d == "claude.ai" || d.ends_with(".claude.ai")
            })
            .map(|c| BrowserCookie {
                name: c["name"].as_str().unwrap_or("").into(),
                value: c["value"].as_str().unwrap_or("").into(),
                domain: c["domain"].as_str().unwrap_or("").into(),
                path: c["path"].as_str().unwrap_or("/").into(),
                secure: c["secure"].as_bool().unwrap_or(false),
                http_only: c["httpOnly"].as_bool().unwrap_or(false),
                expires: c["expires"].as_f64().filter(|e| *e > 0.0).map(|e| e as i64),
            })
            .collect();
        // Once the session cookie appears, give claude.ai a few seconds to set the rest
        // (organization, device ids) before taking them.
        if cookies.iter().any(|c| c.name == "sessionKey" && !c.value.is_empty()) {
            signed_in_at.get_or_insert_with(Instant::now);
        }
        if signed_in_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            id += 1;
            let _ = cdp(&mut ws, id, "Browser.close");
            drop(session);
            return Ok(cookies);
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}
