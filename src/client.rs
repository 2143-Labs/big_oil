//! HTTP client for `GET /public-stash-tabs`.
//!
//! GGG publishes dynamic rate limits in response headers
//! (`X-Rate-Limit-Policy`, `X-Rate-Limit-Rules`, `X-Rate-Limit-<rule>`,
//! `X-Rate-Limit-<rule>-State`) and expects clients to honour them. We keep our
//! own send history and never exceed a published limit; 429s are backed off via
//! `Retry-After`.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::user_agent;
use crate::model::StashPage;

pub const DEFAULT_BASE: &str = "https://api.pathofexile.com/public-stash-tabs";
/// The trade site's view of the current stream position (undocumented; no
/// auth). `psapi` is a `next_change_id` at the live edge.
pub const HEAD_URL: &str = "https://www.pathofexile.com/api/trade/data/change-ids";
const MAX_ATTEMPTS: u32 = 6;
/// Never hammer the endpoint even when no limits have been observed yet.
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// The API answered with a status the client does not retry (or no longer
/// retries). Callers branch on `code`.
#[derive(Debug)]
pub struct HttpStatus {
    pub code: u16,
    pub message: String,
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpStatus {}

/// A 200 response whose body is not a stash page. The body is kept so it can
/// be saved, and the next cursor when it can still be read.
#[derive(Debug)]
pub struct BadPage {
    pub change_id: Option<String>,
    pub next_change_id: Option<String>,
    pub body: String,
    pub reason: String,
}

impl std::fmt::Display for BadPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unparseable page (change_id={:?}): {}",
            self.change_id, self.reason
        )
    }
}

impl std::error::Error for BadPage {}

fn http_error(code: u16, message: String) -> anyhow::Error {
    HttpStatus { code, message }.into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Limit {
    max: u32,
    period: u64,
    restrict: u64,
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    sent: VecDeque<Instant>,
    limits: Vec<Limit>,
    restricted_until: Option<Instant>,
    min_interval: Duration,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            min_interval: DEFAULT_MIN_INTERVAL,
            ..Default::default()
        }
    }

    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    pub(crate) fn note(&mut self) {
        self.sent.push_back(Instant::now());
    }

    /// True when a 429 told us to wait (so the loop must not invent its own
    /// backoff on top).
    pub(crate) fn is_restricted(&self) -> bool {
        self.restricted_until.is_some_and(|t| t > Instant::now())
    }

    /// How long we must wait before the next request is within every limit.
    pub(crate) fn delay(&mut self) -> Duration {
        let now = Instant::now();
        let longest_period = self
            .limits
            .iter()
            .map(|l| l.period)
            .chain(std::iter::once(60))
            .max()
            .unwrap_or(60);
        while let Some(front) = self.sent.front() {
            if now.duration_since(*front).as_secs() > longest_period {
                self.sent.pop_front();
            } else {
                break;
            }
        }

        let mut wait = match self.restricted_until {
            Some(t) if t > now => t - now,
            _ => Duration::ZERO,
        };
        if let Some(last) = self.sent.back() {
            let since = now.duration_since(*last);
            if since < self.min_interval {
                wait = wait.max(self.min_interval - since);
            }
        }
        for limit in &self.limits {
            let window = Duration::from_secs(limit.period);
            let in_window = self
                .sent
                .iter()
                .filter(|t| now.duration_since(**t) < window)
                .count() as u32;
            if in_window >= limit.max
                && let Some(oldest) = self.sent.iter().find(|t| now.duration_since(**t) < window)
            {
                let free_at = *oldest + window;
                if free_at > now {
                    wait = wait.max(free_at - now);
                }
            }
        }
        wait
    }

    pub(crate) fn observe(&mut self, headers: &ureq::http::HeaderMap) {
        let header = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };

        if let Some(retry) = header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()) {
            self.restricted_until = Some(Instant::now() + Duration::from_secs(retry));
        }

        let rules = header("x-rate-limit-rules").unwrap_or_else(|| "Ip".into());
        let mut limits = Vec::new();
        for rule in rules.split(',') {
            let rule = rule.trim().to_ascii_lowercase();
            if rule.is_empty() {
                continue;
            }
            if let Some(value) = header(&format!("x-rate-limit-{rule}")) {
                limits.extend(parse_limits(&value));
            }
            if let Some(state) = header(&format!("x-rate-limit-{rule}-state")) {
                let state = parse_limits(&state);
                // `active` (third field) means this rule is currently restricting us.
                if let Some(active) = state.iter().map(|l| l.restrict).max().filter(|v| *v > 0) {
                    self.restricted_until = Some(Instant::now() + Duration::from_secs(active));
                }
                // The server counts requests we did not see (another process,
                // an earlier run): adopt its count so we do not overshoot.
                for window in &state {
                    let now = Instant::now();
                    let span = Duration::from_secs(window.period);
                    let local = self
                        .sent
                        .iter()
                        .filter(|t| now.duration_since(**t) < span)
                        .count() as u32;
                    for _ in local..window.max {
                        self.sent.push_back(now);
                    }
                }
            }
        }
        if !limits.is_empty() {
            self.limits = limits;
        }
    }
}

fn parse_limits(raw: &str) -> Vec<Limit> {
    raw.split(',')
        .filter_map(|part| {
            let mut it = part.trim().split(':');
            let max = it.next()?.trim().parse().ok()?;
            let period = it.next()?.trim().parse().ok()?;
            let restrict = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
            Some(Limit {
                max,
                period,
                restrict,
            })
        })
        .collect()
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
    user_agent: String,
    limiter: RateLimiter,
    pub total_wait: Duration,
}

impl Client {
    pub fn new(
        base: impl Into<String>,
        token: impl Into<String>,
        contact: &str,
        client_id: &str,
    ) -> Self {
        // We inspect status codes ourselves: 429 has meaning beyond "error".
        let agent = agent(Duration::from_secs(60));
        Client {
            agent,
            base: base.into(),
            token: token.into(),
            user_agent: user_agent(client_id, contact),
            limiter: RateLimiter::new(),
            total_wait: Duration::ZERO,
        }
    }

    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.limiter = self.limiter.with_min_interval(interval);
        self
    }

    /// Swap in a fresh access token, keeping the learned rate limits.
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// Fetch the page starting at `change_id` (or the oldest page when `None`).
    /// Retries rate limiting and transient failures internally; gives up with
    /// an [`HttpStatus`] or [`BadPage`] error.
    pub fn fetch(&mut self, change_id: Option<&str>, realm: Option<&str>) -> Result<StashPage> {
        let mut url = self.base.clone();
        if let Some(realm) = realm {
            url.push('/');
            url.push_str(&urlencode(realm));
        }
        if let Some(id) = change_id {
            url.push_str("?id=");
            url.push_str(&urlencode(id));
        }

        let mut attempt = 0;
        loop {
            attempt += 1;
            let wait = self.limiter.delay();
            if !wait.is_zero() {
                self.total_wait += wait;
                std::thread::sleep(wait);
            }

            self.limiter.note();
            let result = self
                .agent
                .get(&url)
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("User-Agent", &self.user_agent)
                .header("Accept", "application/json")
                .call();

            let mut resp = match result {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(e).context("fetching public stash page");
                    }
                    let backoff = Duration::from_secs(1 << attempt.min(5));
                    self.total_wait += backoff;
                    tracing::warn!(error = %e, ?backoff, "request failed, retrying");
                    std::thread::sleep(backoff);
                    continue;
                }
            };
            self.limiter.observe(resp.headers());

            let status = resp.status().as_u16();
            match status {
                200 => {
                    let text = resp
                        .body_mut()
                        .read_to_string()
                        .context("reading public stash page body")?;
                    return StashPage::parse(text.as_bytes()).map_err(|e| {
                        let next_change_id = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v["next_change_id"].as_str().map(str::to_owned));
                        BadPage {
                            change_id: change_id.map(str::to_owned),
                            next_change_id,
                            reason: format!("{e:#} ({} bytes)", text.len()),
                            body: text,
                        }
                        .into()
                    });
                }
                400 => {
                    let body = resp.body_mut().read_to_string().unwrap_or_default();
                    return Err(http_error(
                        400,
                        format!("HTTP 400: the API rejected the cursor: {}", body.trim()),
                    ));
                }
                401 => {
                    return Err(http_error(
                        401,
                        "401 Unauthorized: the access token was rejected.\n\
                         Tokens can be revoked from the application settings page. Run `big_oil auth --force`."
                            .into(),
                    ));
                }
                403 => {
                    let body = resp.body_mut().read_to_string().unwrap_or_default();
                    return Err(http_error(
                        403,
                        format!(
                            "403 Forbidden: {}\n\
                             The application needs the `service:psapi` scope (client_credentials grant).",
                            body.trim()
                        ),
                    ));
                }
                429 => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(http_error(
                            429,
                            format!("429 Too Many Requests after {attempt} attempts; backing off"),
                        ));
                    }
                    // `observe` already set restricted_until from Retry-After;
                    // fall back to exponential backoff if the header was absent.
                    if !self.limiter.is_restricted() {
                        let backoff = Duration::from_secs(5 * attempt as u64);
                        self.total_wait += backoff;
                        std::thread::sleep(backoff);
                    }
                }
                500..=599 => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(http_error(
                            status,
                            format!("server error {status} after {attempt} attempts"),
                        ));
                    }
                    let backoff = Duration::from_secs(1 << attempt.min(5));
                    self.total_wait += backoff;
                    tracing::warn!(status, ?backoff, "server error, retrying");
                    std::thread::sleep(backoff);
                }
                other => {
                    let body = resp.body_mut().read_to_string().unwrap_or_default();
                    return Err(http_error(
                        other,
                        format!("unexpected HTTP {other}: {}", body.trim()),
                    ));
                }
            }
        }
    }
}

/// The current `next_change_id` at the live edge of the stream, from the trade
/// site (no auth).
pub fn fetch_head(url: &str, user_agent: &str) -> Result<String> {
    let failed = || format!("could not read the current change id from {url}");
    let mut resp = agent(Duration::from_secs(30))
        .get(url)
        .header("User-Agent", user_agent)
        .header("Accept", "application/json")
        .call()
        .with_context(failed)?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        let excerpt: String = body.trim().chars().take(200).collect();
        bail!("{}: HTTP {status}: {excerpt}", failed());
    }
    let value: Value = serde_json::from_str(&body).with_context(failed)?;
    value["psapi"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{}: the response has no psapi id", failed()))
}

/// An HTTP agent that leaves status handling to the caller (429 has meaning
/// beyond "error") and gives up after `timeout`.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

/// Percent-encode a query or form value (RFC 3986 unreserved set kept).
pub fn urlencode(s: &str) -> String {
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
    fn parses_policy_headers() {
        let limits = parse_limits("8:6:60,15:60:300,30:300:1800");
        assert_eq!(limits.len(), 3);
        assert_eq!(
            limits[0],
            Limit {
                max: 8,
                period: 6,
                restrict: 60
            }
        );
        assert_eq!(
            limits[2],
            Limit {
                max: 30,
                period: 300,
                restrict: 1800
            }
        );
    }

    #[test]
    fn waits_once_a_limit_is_reached() {
        let mut limiter = RateLimiter::new().with_min_interval(Duration::ZERO);
        limiter.limits = vec![Limit {
            max: 2,
            period: 60,
            restrict: 60,
        }];
        limiter.note();
        limiter.note();
        assert!(limiter.delay() > Duration::from_secs(50));
    }

    #[test]
    fn respects_min_interval() {
        let mut limiter = RateLimiter::new().with_min_interval(Duration::from_millis(500));
        limiter.note();
        let wait = limiter.delay();
        assert!(wait > Duration::from_millis(100) && wait <= Duration::from_millis(500));
    }

    #[test]
    fn adopts_the_servers_hit_count() {
        use ureq::http::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("x-rate-limit-rules", HeaderValue::from_static("Ip"));
        headers.insert("x-rate-limit-ip", HeaderValue::from_static("6:60:60"));
        headers.insert("x-rate-limit-ip-state", HeaderValue::from_static("5:60:0"));

        let mut limiter = RateLimiter::new().with_min_interval(Duration::ZERO);
        limiter.observe(&headers);
        assert!(limiter.delay().is_zero(), "one request left in the window");
        limiter.note();
        assert!(limiter.delay() > Duration::from_secs(50));
    }
}
