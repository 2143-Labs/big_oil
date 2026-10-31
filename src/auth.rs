//! OAuth 2.1 client-credentials grant for the Public Stash API.
//!
//! Client-credentials tokens have no expiry until revoked, so the token is
//! cached on disk (0600) and reused.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::user_agent;

pub const TOKEN_URL: &str = "https://www.pathofexile.com/oauth/token";
pub const SCOPE: &str = "service:psapi";

#[derive(Debug, Clone, Deserialize, Serialize)]
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

pub fn save_cached(path: &Path, token: &Token) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(token)?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
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
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent();
    let mut resp = agent
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
    let mut token: Token = serde_json::from_str(&text)
        .with_context(|| format!("parsing OAuth token response: {}", text.trim()))?;
    token.obtained_at = crate::store::unix_now();
    token.client_id = client_id.to_owned();
    Ok(token)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencodes_form_values() {
        assert_eq!(urlencode("a b+c/d"), "a%20b%2Bc%2Fd");
    }

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
