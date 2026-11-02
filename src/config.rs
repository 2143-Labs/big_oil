//! Config, credential and path resolution.
//!
//! Credentials never live in the repository or the binary: they come from the
//! environment or from `$XDG_CONFIG_HOME/big_oil/config.json`, matching GGG's
//! rule that keys must not be embedded in code or distributed binaries.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ConfigFile {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Full-account session cookie for reading your own stash tabs.
    pub poesessid: Option<String>,
    /// Account name (`Name#1234`) used for own-stash reads.
    pub account: Option<String>,
    /// Contact string for the required `User-Agent: OAuth <id>/<ver> (contact: ...)`.
    pub contact: Option<String>,
    /// Public stash stream database.
    pub db_path: Option<PathBuf>,
    /// Own-tab database (your tabs and the poe.ninja snapshot).
    pub own_db_path: Option<PathBuf>,
    pub interval_secs: Option<u64>,
}

/// `Some("<redacted>")` / `None`: shows whether a secret is set, never what it is.
fn redacted(secret: &Option<String>) -> Option<&'static str> {
    secret.as_ref().map(|_| "<redacted>")
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("client_id", &self.client_id)
            .field("client_secret", &redacted(&self.client_secret))
            .field("poesessid", &redacted(&self.poesessid))
            .field("account", &self.account)
            .field("contact", &self.contact)
            .field("db_path", &self.db_path)
            .field("own_db_path", &self.own_db_path)
            .field("interval_secs", &self.interval_secs)
            .finish()
    }
}

#[derive(Clone)]
pub struct Config {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub poesessid: Option<String>,
    pub account: Option<String>,
    pub contact: Option<String>,
    /// Public stash stream database.
    pub db_path: PathBuf,
    /// Own tabs and the poe.ninja snapshot.
    pub own_db_path: PathBuf,
    pub interval_secs: u64,
    pub config_dir: PathBuf,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("client_id", &self.client_id)
            .field("client_secret", &redacted(&self.client_secret))
            .field("poesessid", &redacted(&self.poesessid))
            .field("account", &self.account)
            .field("contact", &self.contact)
            .field("db_path", &self.db_path)
            .field("own_db_path", &self.own_db_path)
            .field("interval_secs", &self.interval_secs)
            .field("config_dir", &self.config_dir)
            .finish()
    }
}

impl Config {
    pub fn load(db_override: Option<PathBuf>, own_db_override: Option<PathBuf>) -> Result<Config> {
        let config_dir = config_dir()?;
        let file_path = config_dir.join("config.json");
        let file: ConfigFile = match std::fs::read(&file_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", file_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file_path.display())),
        };
        #[cfg(unix)]
        if file.client_secret.is_some() || file.poesessid.is_some() {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&file_path)
                && meta.permissions().mode() & 0o077 != 0
            {
                let path = file_path.display();
                tracing::warn!("{path} is readable by other users; run chmod 600 {path}");
            }
        }

        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let db_path = match db_override
            .or_else(|| env("BIG_OIL_DB").map(PathBuf::from))
            .or(file.db_path)
        {
            Some(path) => path,
            None => data_dir()?.join("market.db"),
        };
        let own_db_path = match own_db_override
            .or_else(|| env("BIG_OIL_OWN_DB").map(PathBuf::from))
            .or(file.own_db_path)
        {
            Some(path) => path,
            None => data_dir()?.join("own.db"),
        };
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
            own_db_path,
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

    /// Unparseable stream pages are kept here for inspection.
    pub fn bad_pages_dir(&self) -> PathBuf {
        self.db_path.with_file_name("bad_pages")
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
            ("market db", self.db_path.display().to_string()),
            ("own db", self.own_db_path.display().to_string()),
            ("bad pages", self.bad_pages_dir().display().to_string()),
        ]
    }
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("BIG_OIL_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    dirs::config_dir()
        .map(|d| d.join("big_oil"))
        .context("cannot determine the config directory; set BIG_OIL_CONFIG_DIR")
}

/// Where the databases live by default (`$XDG_DATA_HOME/big_oil`).
pub fn data_dir() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|d| d.join("big_oil"))
        .context("cannot determine the data directory; set BIG_OIL_DB and BIG_OIL_OWN_DB")
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn user_agent(client_id: &str, contact: &str) -> String {
    format!("OAuth {client_id}/{VERSION} (contact: {contact}) big_oil")
}

/// User-Agent for calls that are not made with an OAuth client (poe.ninja, the
/// website's own stash endpoint).
pub fn app_user_agent(contact: &str) -> String {
    format!("big_oil/{VERSION} (contact: {contact})")
}

/// Credentials and session cookies only ever travel over https; plain http is
/// allowed for a test server on this machine.
pub fn check_base_url(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|host| {
            lower.starts_with(host)
                && matches!(lower.as_bytes().get(host.len()), None | Some(b':' | b'/'))
        });
    if lower.starts_with("https://") || local {
        return Ok(());
    }
    bail!(
        "refusing {url}: credentials and session cookies are only sent over https \
         (plain http is allowed for localhost test servers)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_must_be_https_or_local() {
        for ok in [
            "https://www.pathofexile.com",
            "http://127.0.0.1:8765",
            "http://localhost/x",
            "http://[::1]:9",
        ] {
            assert!(check_base_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "ftp://x",
        ] {
            let err = check_base_url(bad).unwrap_err().to_string();
            assert!(err.starts_with(&format!("refusing {bad}")), "{err}");
        }
    }

    #[test]
    fn debug_hides_secrets() {
        let file = ConfigFile {
            client_secret: Some("client-sekrit".into()),
            poesessid: Some("session-sekrit".into()),
            ..Default::default()
        };
        let shown = format!("{file:?}");
        assert!(!shown.contains("sekrit"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }
}
