//! One long-lived, auto-reconnecting WebSocket connection per host.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use rustyclaude_protocol::{ClientMsg, DaemonMsg, PROTOCOL_VERSION};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::hosts::HostConfig;

#[derive(Clone, Serialize)]
pub struct HostStatus {
    pub host: String,
    /// "connecting" | "connected" | "disconnected"
    pub status: String,
    pub error: Option<String>,
}

pub struct HostHandle {
    pub url: String,
    pub tx: mpsc::UnboundedSender<ClientMsg>,
    pub status: Arc<Mutex<HostStatus>>,
}

#[derive(Serialize, Clone)]
struct MsgEvent<'a> {
    host: &'a str,
    msg: DaemonMsg,
}

fn set_status(app: &AppHandle, slot: &Mutex<HostStatus>, status: &str, error: Option<String>) {
    let s = {
        let mut g = slot.lock().unwrap();
        g.status = status.into();
        g.error = error;
        g.clone()
    };
    let _ = app.emit_to(crate::SIDEBAR, "host-status", s);
}

pub fn spawn(app: AppHandle, cfg: HostConfig) -> HostHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let status = Arc::new(Mutex::new(HostStatus {
        host: cfg.name.clone(),
        status: "connecting".into(),
        error: None,
    }));
    let handle = HostHandle { url: cfg.url.clone(), tx, status: status.clone() };

    tauri::async_runtime::spawn(async move {
        let mut backoff = 1u64;
        loop {
            // Drop input queued while we were offline; it targeted a dead attach.
            while rx.try_recv().is_ok() {}
            set_status(&app, &status, "connecting", None);
            let mut connected = false;
            let err = run(&app, &cfg, &status, &mut rx, &mut connected).await.err();
            if rx.is_closed() {
                return;
            }
            set_status(&app, &status, "disconnected", err.map(|e| format!("{e:#}")));
            if connected {
                backoff = 1;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    });
    handle
}

async fn run(
    app: &AppHandle,
    cfg: &HostConfig,
    status: &Mutex<HostStatus>,
    rx: &mut mpsc::UnboundedReceiver<ClientMsg>,
    connected: &mut bool,
) -> Result<()> {
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 << 20))
        .max_frame_size(Some(128 << 20));
    let (ws, _) =
        tokio_tungstenite::connect_async_with_config(cfg.url.as_str(), Some(ws_config), false).await?;
    let (mut sink, mut stream) = ws.split();

    async fn send<S: SinkExt<Message> + Unpin>(sink: &mut S, m: &ClientMsg) -> Result<()>
    where
        S::Error: std::error::Error + Send + Sync + 'static,
    {
        sink.send(Message::text(serde_json::to_string(m)?)).await?;
        Ok(())
    }

    send(&mut sink, &ClientMsg::Hello { token: cfg.token.clone(), protocol: PROTOCOL_VERSION }).await?;

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { return Ok(()) };
                let text = match frame? {
                    Message::Text(t) => t,
                    Message::Close(_) => return Ok(()),
                    _ => continue,
                };
                let msg: DaemonMsg = serde_json::from_str(text.as_str())?;
                match &msg {
                    DaemonMsg::Welcome { .. } => {
                        *connected = true;
                        set_status(app, status, "connected", None);
                        send(&mut sink, &ClientMsg::ListAccounts).await?;
                        send(&mut sink, &ClientMsg::ListSessions).await?;
                    }
                    DaemonMsg::Error { message } if !*connected => bail!("{message}"),
                    _ => {}
                }
                app.emit_to(crate::SIDEBAR, "daemon-msg", MsgEvent { host: &cfg.name, msg })?;
            }
            out = rx.recv() => {
                let Some(out) = out else { return Ok(()) };
                send(&mut sink, &out).await?;
            }
        }
    }
}
