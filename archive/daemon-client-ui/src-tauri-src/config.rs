use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    pub name: String,
    /// e.g. ws://100.64.0.5:7878 (Tailscale IP) or wss://...
    pub url: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default, rename = "host")]
    pub hosts: Vec<HostConfig>,
}

const EXAMPLE: &str = r#"# RustyClaude hosts. One [[host]] block per machine running rustyclaude-daemon.
# Restart the app after editing.

[[host]]
name = "local-dev"
url = "ws://127.0.0.1:7878"
token = "dev"

# [[host]]
# name = "vps"
# url = "ws://100.x.y.z:7878"   # Tailscale IP of the VPS
# token = "long-random-secret"
"#;

pub fn path() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("rustyclaude").join("hosts.toml"))
}

pub fn load() -> Result<Config> {
    let p = path()?;
    if !p.exists() {
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, EXAMPLE)?;
    }
    let text = std::fs::read_to_string(&p)?;
    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}
