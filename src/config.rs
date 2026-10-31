//! Config, credential and path resolution.
//!
//! Credentials never live in the repository or the binary: they come from the
//! environment or from `$XDG_CONFIG_HOME/big_oil/config.json`, matching GGG's
//! rule that keys must not be embedded in code or distributed binaries.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigFile {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Full-account session cookie for reading your own stash tabs.
    pub poesessid: Option<String>,
    /// Account name (`Name#1234`) used for own-stash reads.
    pub account: Option<String>,
    /// Contact string for the required `User-Agent: OAuth <id>/<ver> (contact: ...)`.
    pub contact: Option<String>,
    pub db_path: Option<PathBuf>,
    pub interval_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub poesessid: Option<String>,
    pub account: Option<String>,
    pub contact: Option<String>,
    pub db_path: PathBuf,
    pub interval_secs: u64,
    pub config_dir: PathBuf,
}

impl Config {
    pub fn load(db_override: Option<PathBuf>) -> Result<Config> {
        let config_dir = config_dir();
        let file_path = config_dir.join("config.json");
        let file: ConfigFile = match std::fs::read(&file_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", file_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file_path.display())),
        };

        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let db_path = db_override
            .or_else(|| env("BIG_OIL_DB").map(PathBuf::from))
            .or(file.db_path)
            .unwrap_or_else(default_db_path);
        let interval_secs = env("BIG_OIL_INTERVAL")
            .and_then(|v| v.parse().ok())
            .or(file.interval_secs)
            .unwrap_or(10)
            .max(1);

        Ok(Config {
            client_id: env("BIG_OIL_CLIENT_ID").or(file.client_id),
            client_secret: env("BIG_OIL_CLIENT_SECRET").or(file.client_secret),
            poesessid: env("BIG_OIL_POESESSID").or(file.poesessid),
            account: env("BIG_OIL_ACCOUNT").or(file.account),
            contact: env("BIG_OIL_CONTACT").or(file.contact),
            db_path,
            interval_secs,
            config_dir,
        })
    }

    pub fn token_path(&self) -> PathBuf {
        self.config_dir.join("token.json")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    /// Returns the client credentials, or a message explaining exactly what is
    /// missing and where to put it.
    pub fn credentials(&self) -> Result<(&str, &str)> {
        let id = self.client_id.as_deref().unwrap_or_default();
        let secret = self.client_secret.as_deref().unwrap_or_default();
        if id.is_empty() || secret.is_empty() {
            bail!(
                "no OAuth client configured.\n\
                 The Public Stash API requires a registered OAuth application with the\n\
                 `service:psapi` scope (guest access was removed in 3.20.0).\n\
                 Put this in {}:\n\
                 {{\n  \"client_id\": \"...\",\n  \"client_secret\": \"...\",\n  \"contact\": \"you@example.com\"\n}}\n\
                 or set BIG_OIL_CLIENT_ID / BIG_OIL_CLIENT_SECRET / BIG_OIL_CONTACT.\n\
                 Existing applications are managed at\n\
                 https://www.pathofexile.com/my-account (profile → Manage applications).",
                self.config_path().display()
            );
        }
        Ok((id, secret))
    }

    pub fn contact(&self) -> Result<&str> {
        match self.contact.as_deref() {
            Some(c) if !c.is_empty() => Ok(c),
            _ => bail!(
                "no contact configured: GGG requires `User-Agent: OAuth <client_id>/<version> (contact: <email>)`.\n\
                 Set \"contact\" in {} or BIG_OIL_CONTACT.",
                self.config_path().display()
            ),
        }
    }

    /// Non-secret description of where things live.
    pub fn layout(&self) -> Vec<(&'static str, String)> {
        vec![
            ("config", self.config_path().display().to_string()),
            ("token", self.token_path().display().to_string()),
            ("database", self.db_path.display().to_string()),
        ]
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BIG_OIL_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("big_oil")
}

pub fn default_db_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("big_oil").join("market.db")
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn user_agent(client_id: &str, contact: &str) -> String {
    format!("OAuth {client_id}/{VERSION} (contact: {contact}) big_oil")
}
