use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_START_URL: &str = "https://claude.ai/code";
const COLORS: &[&str] = &["#d97757", "#6a9bcc", "#788c5d", "#b07cc6", "#c9a227", "#5fa8a0", "#c26d7e"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// 32 hex chars. Also the 16-byte WebKit data store id, so it must never change.
    pub id: String,
    pub name: String,
    pub color: String,
}

impl Account {
    #[cfg(target_os = "macos")]
    pub fn store_id(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(self.id.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
        }
        out
    }

    pub fn label(&self) -> String {
        format!("acct-{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_start_url")]
    pub start_url: String,
    #[serde(default)]
    pub last_active: Option<String>,
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

fn default_start_url() -> String {
    DEFAULT_START_URL.into()
}

impl Default for Config {
    fn default() -> Self {
        Self { start_url: default_start_url(), last_active: None, accounts: vec![] }
    }
}

impl Config {
    pub fn new_account(&self, name: &str) -> Account {
        let color = COLORS[self.accounts.len() % COLORS.len()].to_string();
        Account { id: uuid::Uuid::new_v4().simple().to_string(), name: name.trim().to_string(), color }
    }
}

pub fn path() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("rustyclaude").join("accounts.toml"))
}

pub fn load() -> Result<Config> {
    let p = path()?;
    if !p.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&p)?;
    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

pub fn save(cfg: &Config) -> Result<()> {
    let p = path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(cfg)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}
