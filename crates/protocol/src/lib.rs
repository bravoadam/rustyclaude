//! Wire protocol between the RustyClaude client(s) and the per-host daemon.
//!
//! Transport: WebSocket, one JSON object per text frame, tagged by `type`.
//! Terminal bytes are base64-encoded so arbitrary (non-UTF-8) output survives.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 4;

/// Largest file the daemon will send back for previewing.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// How a session runs on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Interactive `claude` in a PTY. Usable from the official Claude apps via Remote Control.
    #[default]
    Terminal,
    /// `claude -p` with stream-json I/O. Rich chat UI: attachments, inline media, tool cards.
    Chat,
}

/// A file sent along with a chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Base64 contents.
    pub data: String,
}

/// Account name meaning "use the default ~/.claude config dir".
pub const DEFAULT_ACCOUNT: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub account: String,
    pub cwd: String,
    /// Unix seconds.
    pub started_at: u64,
    pub running: bool,
    #[serde(default)]
    pub mode: SessionMode,
    /// Terminal mode: the output says this account hit its usage limit.
    #[serde(default)]
    pub limited: bool,
    /// Terminal mode: an account switch (/login) is in progress.
    #[serde(default)]
    pub logging_in: bool,
    /// In the daemon's session list (autostarted, restarted on crash). False for
    /// tmux sessions the daemon only found running.
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub autostart: bool,
    /// Claude is asking to log in (or has no credentials yet).
    #[serde(default)]
    pub needs_login: bool,
    /// Managed sessions: start with `--continue` (resume the folder's last conversation).
    #[serde(default)]
    pub resume: bool,
    /// Managed sessions: start with `--dangerously-skip-permissions`.
    #[serde(default)]
    pub skip_permissions: bool,
}

/// Client -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Must be the first message on a connection.
    Hello { token: String, protocol: u32 },
    ListSessions,
    ListAccounts,
    StartSession {
        account: String,
        cwd: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mode: SessionMode,
        /// Chat mode: Claude Code `--permission-mode` (default, acceptEdits, plan, bypassPermissions).
        #[serde(default)]
        permission_mode: Option<String>,
        /// tmux sessions: resume the folder's last conversation on start (default true).
        #[serde(default)]
        resume: Option<bool>,
        /// tmux sessions: run with --dangerously-skip-permissions.
        #[serde(default)]
        skip_permissions: Option<bool>,
    },
    /// Chat mode: send a user turn.
    SendChat { session: String, text: String, #[serde(default)] attachments: Vec<Attachment> },
    /// Terminal mode: switch the session's account by driving `/login` inside it.
    /// `account` is just a label recorded on success. The daemon answers with `LoginUrl`.
    LoginStart { session: String, account: String },
    /// Terminal mode: the authorization code from the sign-in page.
    LoginCode { session: String, code: String },
    /// Terminal mode: abandon an in-progress switch (sends Escape).
    LoginCancel { session: String },
    /// tmux sessions: "start" | "stop" | "restart" | "manage" | "unmanage" |
    /// "autostart_on" | "autostart_off" | "remote_control" | "resume_on" | "resume_off" |
    /// "skip_on" | "skip_off" (the last four apply on the next start).
    SessionAction { session: String, action: String },
    /// Terminal mode: type a line into the session (e.g. "/remote-control").
    Command { session: String, line: String },
    /// Fetch a file from the host for previewing. Relative paths resolve against the session cwd.
    ReadFile { request: String, session: Option<String>, path: String },
    /// Start streaming a session's output (replays recent scrollback first).
    Attach { session: String, cols: u16, rows: u16 },
    Detach { session: String },
    /// Base64 bytes to write to the session's terminal.
    Input { session: String, data: String },
    Resize { session: String, cols: u16, rows: u16 },
    Kill { session: String },
}

/// Daemon -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMsg {
    Welcome { host: String, protocol: u32 },
    Sessions { sessions: Vec<SessionInfo> },
    Accounts { accounts: Vec<String> },
    SessionStarted { session: SessionInfo },
    /// Base64 terminal output.
    Output { session: String, data: String },
    Exited { session: String, code: Option<u32> },
    /// Chat mode: one event. Either a raw Claude Code stream-json line, or one of the
    /// daemon's own: `{"type":"local_user",...}` (echo of a sent message, so every
    /// attached client sees it) and `{"type":"stderr","text":...}`.
    Chat { session: String, event: serde_json::Value },
    File { request: String, path: String, mime: String, data: String },
    FileError { request: String, message: String },
    /// Open this URL, signed in as the target account, and approve.
    LoginUrl { session: String, url: String },
    /// "waiting_code" | "success" | "failed"
    LoginStatus { session: String, status: String, #[serde(default)] message: Option<String> },
    Error { message: String },
}

pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_tagging() {
        let m = ClientMsg::Attach { session: "s1".into(), cols: 80, rows: 24 };
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(j, r#"{"type":"attach","session":"s1","cols":80,"rows":24}"#);
        let unit: ClientMsg = serde_json::from_str(r#"{"type":"list_sessions"}"#).unwrap();
        assert!(matches!(unit, ClientMsg::ListSessions));
    }
}
