//! Reading the account's own stash tabs.
//!
//! The documented OAuth route (`GET /stash/{league}`, scope `account:stashes`)
//! needs a registered application, so this uses the website's own call instead:
//! `GET /character-window/get-stash-items?accountName=…&league=…&tabIndex=N`
//! with a logged-in `POESESSID` cookie.
//!
//! That endpoint is not part of GGG's documented API, and GGG's terms (section
//! 7i) forbid using undocumented endpoints; account termination is the stated
//! risk. Requests are paced (at least 1.5 s apart, published rate limits
//! honoured, 429 backed off). The cookie is a full account session, so it is
//! only ever read from the environment or the config file, sent to
//! pathofexile.com, and never logged.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::client::{RateLimiter, agent, urlencode};
use crate::config::app_user_agent;
use crate::listing::TabSelector;
use crate::model::Item;
use crate::store::{OwnTab, Store};

pub const BASE: &str = "https://www.pathofexile.com";
const MAX_ATTEMPTS: u32 = 3;

/// The session cookie no longer works; retrying cannot help.
#[derive(Debug)]
pub struct SessionExpired {
    pub detail: String,
}

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SessionExpired {}

const FRESH_COOKIE: &str = "Copy a fresh POESESSID from a logged-in pathofexile.com tab (DevTools → \
     Application → Cookies) into BIG_OIL_POESESSID, or put \"poesessid\" in the config.";

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

#[derive(Debug, Clone)]
pub struct TabList {
    pub num_tabs: i64,
    pub tabs: Vec<TabInfo>,
    /// The items of tab index 0, which come with the tab list. `None` when that
    /// part of the response is unusable, so tab 0 is never taken as empty.
    pub first_items: Option<Vec<Item>>,
}

pub struct OwnClient {
    agent: ureq::Agent,
    base: String,
    cookie: String,
    user_agent: String,
    limiter: RateLimiter,
}

impl OwnClient {
    pub fn new(cookie: impl Into<String>, contact: &str) -> Self {
        Self::with_base(BASE, cookie, contact)
    }

    pub fn with_base(base: impl Into<String>, cookie: impl Into<String>, contact: &str) -> Self {
        OwnClient {
            agent: agent(Duration::from_secs(45)),
            base: base.into(),
            cookie: cookie.into(),
            user_agent: app_user_agent(contact),
            limiter: RateLimiter::new().with_min_interval(Duration::from_millis(1500)),
        }
    }

    /// Minimum gap between requests (default 1.5 s).
    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.limiter = self.limiter.with_min_interval(interval);
        self
    }

    /// Tab list plus the items of tab 0, which is how the website asks for it.
    pub fn tabs(&mut self, account: &str, league: &str) -> Result<TabList> {
        let value = self.get(account, league, 0, true)?;
        let mut list = parse_tab_list(&value)?;
        list.first_items = parse_items(&value).ok();
        Ok(list)
    }

    pub fn tab(&mut self, account: &str, league: &str, index: i64) -> Result<Vec<Item>> {
        let value = self.get(account, league, index, false)?;
        parse_items(&value)
    }

    fn get(&mut self, account: &str, league: &str, index: i64, list_tabs: bool) -> Result<Value> {
        let url = format!(
            "{}/character-window/get-stash-items?accountName={}&league={}&tabs={}&tabIndex={}",
            self.base,
            urlencode(account),
            urlencode(league),
            i64::from(list_tabs),
            index
        );
        let mut attempt = 0;
        loop {
            attempt += 1;
            let wait = self.limiter.delay();
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            self.limiter.note();
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
            self.limiter.observe(resp.headers());

            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            match status {
                429 if attempt < MAX_ATTEMPTS => {
                    // `observe` set the wait from Retry-After when it was sent.
                    if !self.limiter.is_restricted() {
                        std::thread::sleep(Duration::from_secs(10 * u64::from(attempt)));
                    }
                }
                429 => bail!(
                    "HTTP 429 from the stash endpoint after {attempt} attempts. Poll less often."
                ),
                200 if body.trim_start().starts_with('<') => {
                    return Err(SessionExpired {
                        detail: format!(
                            "the stash endpoint returned HTML: the POESESSID is missing, expired, \
                             or the account name is wrong.\n{FRESH_COOKIE}"
                        ),
                    }
                    .into());
                }
                200 => {
                    return serde_json::from_str(&body).with_context(|| {
                        format!("parsing stash response: {}", truncate(&body, 200))
                    });
                }
                401 | 403 => {
                    return Err(SessionExpired {
                        detail: format!(
                            "HTTP {status} from the stash endpoint: the session cookie was \
                             rejected.\n{FRESH_COOKIE}"
                        ),
                    }
                    .into());
                }
                other => bail!(
                    "stash endpoint returned HTTP {other}: {}",
                    truncate(&body, 300)
                ),
            }
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

/// An `error` object in a 200 response. Codes 6 (forbidden) and 8
/// (unauthorized) mean the session is gone.
fn api_error(value: &Value) -> Option<anyhow::Error> {
    let err = value.get("error")?;
    Some(match err.get("code").and_then(Value::as_i64) {
        Some(6 | 8) => SessionExpired {
            detail: format!("stash endpoint error: {err}\n{FRESH_COOKIE}"),
        }
        .into(),
        _ => anyhow!("stash endpoint error: {err}"),
    })
}

/// The account's tab list. A response without a `tabs` array is an error, never
/// an empty account: an empty list would close every tab.
pub fn parse_tab_list(value: &Value) -> Result<TabList> {
    if let Some(err) = api_error(value) {
        return Err(err);
    }
    let num_tabs = value.get("numTabs").and_then(Value::as_i64).unwrap_or(0);
    let tabs = value
        .get("tabs")
        .and_then(Value::as_array)
        .context("stash response has no tabs list")?;
    let tabs = tabs
        .iter()
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
        .collect();
    Ok(TabList {
        num_tabs,
        tabs,
        first_items: None,
    })
}

/// The trackable items of one tab. A response without an `items` array is an
/// error, never an empty tab: an empty tab would close every item in it.
pub fn parse_items(value: &Value) -> Result<Vec<Item>> {
    if let Some(err) = api_error(value) {
        return Err(err);
    }
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .context("stash response has no items array")?;
    Ok(items
        .iter()
        .map(Item::from_value)
        .filter(Item::is_trackable)
        .collect())
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub total_tabs: usize,
    /// Stash ids ingested this pass.
    pub scanned: Vec<String>,
    /// `(tab name, error)` for tabs skipped this pass.
    pub failed: Vec<(String, String)>,
    pub dropped: usize,
    pub removed: usize,
    /// Tabs that no longer exist on the account.
    pub closed_tabs: usize,
}

/// One pass over the account's tabs: read the tab list, ingest every visible
/// tab `selectors` picks (all when empty), and close tabs that are gone. A tab
/// that fails is skipped for this pass; an expired session ends the pass.
pub fn scan(
    store: &mut Store,
    client: &mut OwnClient,
    account: &str,
    league: &str,
    selectors: &[TabSelector],
) -> Result<ScanReport> {
    let list = client.tabs(account, league)?;
    let mut report = ScanReport {
        total_tabs: list.tabs.len(),
        ..Default::default()
    };

    // "Missing" means absent from the account, not unselected or hidden. Every
    // account has tabs, so an empty list is a bad response, not a reason to
    // close them all.
    if !list.tabs.is_empty() {
        let present: HashSet<String> = list.tabs.iter().map(|t| t.stash_id(league)).collect();
        report.closed_tabs = store.close_missing_own_tabs(league, &present)?;
        // Tabs get dragged around: keep every known tab's index current, not
        // just the selected ones, so index selectors stay unambiguous.
        let indexes: Vec<(String, i64)> = list
            .tabs
            .iter()
            .map(|t| (t.stash_id(league), t.index))
            .collect();
        store.set_own_tab_indexes(&indexes)?;
    }

    let visible: Vec<&TabInfo> = list.tabs.iter().filter(|t| !t.hidden).collect();
    let selected: Vec<&TabInfo> = visible
        .iter()
        .copied()
        .filter(|t| {
            selectors.is_empty()
                || selectors
                    .iter()
                    .any(|s| s.matches_tab(Some(t.index), &t.name))
        })
        .collect();
    if selected.is_empty() && !selectors.is_empty() {
        let names: Vec<String> = visible
            .iter()
            .map(|t| format!("{}:{}", t.index, t.name))
            .collect();
        bail!("no tab matched --tab; visible tabs: {}", names.join(", "));
    }

    let mut first_items = list.first_items;
    for tab in selected {
        let fetched = if tab.index == 0
            && let Some(items) = first_items.take()
        {
            Ok(items)
        } else {
            client.tab(account, league, tab.index)
        };
        let items = match fetched {
            Ok(items) => items,
            Err(e) if e.downcast_ref::<SessionExpired>().is_some() => return Err(e),
            Err(e) => {
                report.failed.push((tab.name.clone(), format!("{e:#}")));
                continue;
            }
        };

        let stash_id = tab.stash_id(league);
        let stats = store.ingest_own_tab(
            &OwnTab {
                stash_id: &stash_id,
                index: tab.index,
                name: &tab.name,
                stash_type: &tab.kind,
                league,
                account: Some(account),
            },
            &items,
        )?;
        if stats.items_new > 0 || stats.delisted > 0 {
            tracing::info!(
                tab = %tab.name,
                dropped = stats.items_new,
                removed = stats.delisted,
                "tab changed"
            );
        }
        report.dropped += stats.items_new;
        report.removed += stats.delisted;
        report.scanned.push(stash_id);
    }
    Ok(report)
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

        let items = parse_items(&value).unwrap();
        assert_eq!(items.len(), 2, "entries without an id cannot be tracked");
    }

    #[test]
    fn surfaces_api_errors() {
        let err =
            parse_tab_list(&json!({"error": {"code": 8, "message": "Unauthorized"}})).unwrap_err();
        assert!(err.to_string().contains("Unauthorized"));
        assert!(err.downcast_ref::<SessionExpired>().is_some());

        let err = parse_items(&json!({"error": {"code": 1, "message": "Resource not found"}}))
            .unwrap_err();
        assert!(err.downcast_ref::<SessionExpired>().is_none());
        assert!(err.to_string().contains("Resource not found"));
    }

    #[test]
    fn a_response_without_items_is_not_an_empty_tab() {
        let err = parse_items(&json!({"numTabs": 3})).unwrap_err();
        assert!(err.to_string().contains("no items array"));
        assert!(parse_items(&json!({"items": []})).unwrap().is_empty());

        let err = parse_tab_list(&json!({"numTabs": 3, "items": []})).unwrap_err();
        assert!(err.to_string().contains("no tabs list"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let body = "é".repeat(300);
        let cut = truncate(&body, 200);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 201);
    }
}
