//! Machines running rustyclaude-daemon (for the Sessions panel and account moves).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    pub name: String,
    /// e.g. ws://192.168.1.20:7878 on the LAN, later ws://100.x.y.z:7878 over Tailscale
    pub url: String,
    pub token: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct HostsFile {
    #[serde(default, rename = "host")]
    pub hosts: Vec<HostConfig>,
}

const EXAMPLE: &str = r#"# Machines running rustyclaude-daemon. One [[host]] block each; restart the app after editing.
#
# [[host]]
# name = "linux-box"
# url = "ws://192.168.1.20:7878"   # LAN IP of the machine (Tailscale IP later)
# token = "same secret as RUSTYCLAUDE_TOKEN on that machine"
"#;

pub fn path() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("rustyclaude").join("hosts.toml"))
}

pub fn load() -> Result<HostsFile> {
    let p = path()?;
    if !p.exists() {
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, EXAMPLE)?;
    }
    toml::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing {}", p.display()))
}
