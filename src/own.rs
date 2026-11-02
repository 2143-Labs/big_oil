//! Reading the account's own stash tabs.
//!
//! The documented OAuth route (`GET /stash/{league}`, scope `account:stashes`)
//! needs a registered application, so this uses the website's own call instead:
//! `GET /character-window/get-stash-items?accountName=…&league=…&tabIndex=N`
//! with a logged-in `POESESSID` cookie.
//!
//! That endpoint is not part of GGG's documented API. The cookie is a full
//! account session, so it is only ever read from the environment or the config
//! file, sent to pathofexile.com, and never logged.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::client::{agent, urlencode};
use crate::config::app_user_agent;
use crate::model::Item;

pub const BASE: &str = "https://www.pathofexile.com";

#[derive(Debug, Clone, PartialEq)]
pub struct TabInfo {
    pub index: i64,
    pub name: String,
    pub kind: String,
    pub id: Option<String>,
    pub hidden: bool,
}

impl TabInfo {
    /// Stable id for the stash row. The API supplies a uuid per tab; fall back
    /// to league+index for the rare tab type that omits it.
    pub fn stash_id(&self, league: &str) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("own-{league}-{}", self.index))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabList {
    pub num_tabs: i64,
    pub tabs: Vec<TabInfo>,
}

pub struct OwnClient {
    agent: ureq::Agent,
    base: String,
    cookie: String,
    user_agent: String,
}

impl OwnClient {
    pub fn new(cookie: impl Into<String>, contact: &str) -> Self {
        Self::with_base(BASE, cookie, contact)
    }

    pub fn with_base(base: impl Into<String>, cookie: impl Into<String>, contact: &str) -> Self {
        OwnClient {
            agent: agent(std::time::Duration::from_secs(45)),
            base: base.into(),
            cookie: cookie.into(),
            user_agent: app_user_agent(contact),
        }
    }

    /// Tab list plus the items of tab 0, which is how the website asks for it.
    pub fn tabs(&self, account: &str, league: &str) -> Result<TabList> {
        let value = self.get(account, league, 0, true)?;
        parse_tab_list(&value)
    }

    pub fn tab(&self, account: &str, league: &str, index: i64) -> Result<Vec<Item>> {
        let value = self.get(account, league, index, false)?;
        Ok(parse_items(&value))
    }

    fn get(&self, account: &str, league: &str, index: i64, list_tabs: bool) -> Result<Value> {
        let url = format!(
            "{}/character-window/get-stash-items?accountName={}&league={}&tabs={}&tabIndex={}",
            self.base,
            urlencode(account),
            urlencode(league),
            i64::from(list_tabs),
            index
        );
        let mut resp = self
            .agent
            .get(&url)
            .header("Cookie", format!("POESESSID={}", self.cookie))
            .header("User-Agent", &self.user_agent)
            .header("Referer", format!("{}/", self.base))
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Accept", "application/json")
            .call()
            .context("requesting stash items")?;

        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.body_mut().read_to_string().unwrap_or_default();

        match status {
            200 => serde_json::from_str(&body).with_context(|| {
                if body.trim_start().starts_with('<') {
                    "the stash endpoint returned HTML: the POESESSID is missing, expired, or the \
                     account name is wrong"
                        .to_string()
                } else {
                    format!("parsing stash response: {}", truncate(&body, 200))
                }
            }),
            401 | 403 => bail!(
                "HTTP {status} from the stash endpoint: the session cookie was rejected.\n\
                 Copy a fresh POESESSID from a logged-in pathofexile.com tab (DevTools → \
                 Application → Cookies) into BIG_OIL_POESESSID, or put \"poesessid\" in the config."
            ),
            429 => bail!(
                "HTTP 429 from the stash endpoint{}. Poll less often.",
                retry_after
                    .map(|s| format!(" (Retry-After: {s}s)"))
                    .unwrap_or_default()
            ),
            other => bail!(
                "stash endpoint returned HTTP {other}: {}",
                truncate(&body, 300)
            ),
        }
    }
}

fn truncate(body: &str, max: usize) -> String {
    let body = body.trim();
    let cut: String = body.chars().take(max).collect();
    if cut.len() == body.len() {
        body.to_owned()
    } else {
        cut + "…"
    }
}

pub fn parse_tab_list(value: &Value) -> Result<TabList> {
    if let Some(err) = value.get("error") {
        bail!("stash endpoint error: {err}");
    }
    let num_tabs = value.get("numTabs").and_then(Value::as_i64).unwrap_or(0);
    let tabs = value
        .get("tabs")
        .and_then(Value::as_array)
        .map(|tabs| {
            tabs.iter()
                .enumerate()
                .map(|(fallback, tab)| TabInfo {
                    index: tab
                        .get("i")
                        .and_then(Value::as_i64)
                        .unwrap_or(fallback as i64),
                    name: tab
                        .get("n")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    kind: tab
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    id: tab.get("id").and_then(Value::as_str).map(str::to_owned),
                    hidden: tab.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(TabList { num_tabs, tabs })
}

pub fn parse_items(value: &Value) -> Vec<Item> {
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(Item::from_value)
                .filter(Item::is_trackable)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_tab_list() {
        let value = json!({
            "numTabs": 3,
            "tabs": [
                {"n": "Dump", "i": 0, "id": "aaaaaaaa-0000-0000-0000-000000000000",
                 "type": "PremiumStash", "hidden": false},
                {"n": "Maps", "i": 1, "id": null, "type": "MapStash", "hidden": false}
            ],
            "items": [
                {"id": "i1", "typeLine": "Chaos Orb", "frameType": 5},
                {"id": "i2", "typeLine": "Divine Orb", "frameType": 5},
                {"typeLine": "no id", "frameType": 5}
            ]
        });
        let tabs = parse_tab_list(&value).unwrap();
        assert_eq!(tabs.num_tabs, 3);
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(
            tabs.tabs[0].stash_id("Standard"),
            "aaaaaaaa-0000-0000-0000-000000000000"
        );
        assert_eq!(tabs.tabs[1].stash_id("Standard"), "own-Standard-1");

        let items = parse_items(&value);
        assert_eq!(items.len(), 2, "entries without an id cannot be tracked");
    }

    #[test]
    fn surfaces_api_errors() {
        let err = parse_tab_list(&json!({"error": {"code": 8, "message": "Unauthorized"}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unauthorized"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let body = "é".repeat(300);
        let cut = truncate(&body, 200);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 201);
    }
}
