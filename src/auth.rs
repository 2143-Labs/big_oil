//! OAuth 2.1 client-credentials grant for the Public Stash API.
//!
//! Client-credentials tokens have no expiry until revoked, so the token is
//! cached on disk (0600, in a 0700 directory) and reused.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::client::{agent, urlencode};
use crate::config::user_agent;

pub const TOKEN_URL: &str = "https://www.pathofexile.com/oauth/token";
pub const SCOPE: &str = "service:psapi";

#[derive(Clone, Deserialize, Serialize)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    /// `None` for client-credentials tokens.
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub obtained_at: i64,
    /// Cached tokens are only valid for the client that minted them.
    #[serde(default)]
    pub client_id: String,
}

// Hand-written so the token never reaches a log or panic message.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("access_token", &"<redacted>")
            .field("scope", &self.scope)
            .field("username", &self.username)
            .field("expires_in", &self.expires_in)
            .field("obtained_at", &self.obtained_at)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl Token {
    pub fn describe(&self) -> String {
        format!(
            "scope={} account={} acquired={}",
            self.scope.as_deref().unwrap_or("?"),
            self.username.as_deref().unwrap_or("?"),
            time::OffsetDateTime::from_unix_timestamp(self.obtained_at)
                .map(|t| t
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default())
                .unwrap_or_default()
        )
    }

    /// Client-credentials tokens have no expiry; anything with `expires_in`
    /// gets a five-minute safety margin.
    pub fn is_expired(&self, now: i64) -> bool {
        match self.expires_in {
            Some(secs) => now >= self.obtained_at + secs - 300,
            None => false,
        }
    }
}

pub fn load_cached(path: &Path, client_id: &str, now: i64) -> Option<Token> {
    let bytes = std::fs::read(path).ok()?;
    let token: Token = serde_json::from_slice(&bytes).ok()?;
    if token.client_id != client_id || token.is_expired(now) {
        return None;
    }
    Some(token)
}

/// Write the token so it is never readable by others, not even briefly: a
/// fresh 0600 file is written and synced, then renamed over the old one.
pub fn save_cached(path: &Path, token: &Token) -> Result<()> {
    let body = serde_json::to_vec_pretty(token)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&body)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

pub fn request_token(client_id: &str, client_secret: &str, contact: &str) -> Result<Token> {
    let body = format!(
        "client_id={}&client_secret={}&grant_type=client_credentials&scope={}",
        urlencode(client_id),
        urlencode(client_secret),
        SCOPE
    );
    let mut resp = agent(std::time::Duration::from_secs(30))
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", &user_agent(client_id, contact))
        .send(body.as_str())
        .context("requesting OAuth token")?;

    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        bail!(
            "OAuth token request failed with HTTP {status}: {}\n\
             Check client_id/client_secret and that the application has the `service:psapi` scope.",
            text.trim()
        );
    }
    // The body holds the token: never echo it.
    let mut token: Token = serde_json::from_str(&text).context("parsing OAuth token response")?;
    token.obtained_at = crate::store::unix_now();
    token.client_id = client_id.to_owned();
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_token_must_match_client() {
        let dir = tempdir();
        let path = dir.join("token.json");
        let token = Token {
            access_token: "t".into(),
            scope: Some(SCOPE.into()),
            username: None,
            expires_in: None,
            obtained_at: 1_000,
            client_id: "abc".into(),
        };
        save_cached(&path, &token).unwrap();
        assert!(load_cached(&path, "abc", 2_000).is_some());
        assert!(load_cached(&path, "other", 2_000).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn cached_token_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let path = dir.join("cfg").join("token.json");
        let token = Token {
            access_token: "t".into(),
            scope: None,
            username: None,
            expires_in: None,
            obtained_at: 0,
            client_id: "abc".into(),
        };
        save_cached(&path, &token).unwrap();
        // Saving again replaces the file rather than failing on create_new.
        save_cached(&path, &token).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn debug_never_shows_the_token() {
        let token = Token {
            access_token: "sekrit-access-token".into(),
            scope: None,
            username: None,
            expires_in: None,
            obtained_at: 0,
            client_id: "abc".into(),
        };
        let shown = format!("{token:?}");
        assert!(!shown.contains("sekrit-access-token"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn expiring_tokens_are_refreshed_early() {
        let token = Token {
            access_token: "t".into(),
            scope: None,
            username: None,
            expires_in: Some(600),
            obtained_at: 0,
            client_id: "abc".into(),
        };
        assert!(!token.is_expired(100));
        assert!(token.is_expired(400));
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "big_oil_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
