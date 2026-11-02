#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod conn;

use rustyclaude_protocol::ClientMsg;
use serde::Serialize;
use tauri::{Manager, State};

struct Hosts(Vec<(String, conn::HostHandle)>);

#[derive(Serialize)]
struct HostView {
    name: String,
    url: String,
    status: conn::HostStatus,
}

#[tauri::command]
fn list_hosts(hosts: State<Hosts>) -> Vec<HostView> {
    hosts
        .0
        .iter()
        .map(|(name, h)| HostView { name: name.clone(), url: h.url.clone(), status: h.status.lock().unwrap().clone() })
        .collect()
}

#[tauri::command]
fn send(host: String, msg: ClientMsg, hosts: State<Hosts>) -> Result<(), String> {
    let (_, h) = hosts.0.iter().find(|(n, _)| *n == host).ok_or_else(|| format!("unknown host '{host}'"))?;
    h.tx.send(msg).map_err(|_| "connection task stopped".to_string())
}

#[tauri::command]
fn config_path() -> Result<String, String> {
    config::path().map(|p| p.display().to_string()).map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let cfg = config::load()?;
            let hosts = cfg
                .hosts
                .into_iter()
                .map(|h| (h.name.clone(), conn::spawn(app.handle().clone(), h)))
                .collect();
            app.manage(Hosts(hosts));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![list_hosts, send, config_path])
        .run(tauri::generate_context!())
        .expect("error while running RustyClaude");
}
