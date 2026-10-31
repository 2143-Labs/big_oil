//! HTTP client for `GET /public-stash-tabs`.
//!
//! GGG publishes dynamic rate limits in response headers
//! (`X-Rate-Limit-Policy`, `X-Rate-Limit-Rules`, `X-Rate-Limit-<rule>`,
//! `X-Rate-Limit-<rule>-State`) and expects clients to honour them. We keep our
//! own send history and never exceed a published limit; 429s are backed off via
//! `Retry-After`.

use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::user_agent;
use crate::model::StashPage;

pub const DEFAULT_BASE: &str = "https://api.pathofexile.com/public-stash-tabs";
const MAX_ATTEMPTS: u32 = 6;
/// Never hammer the endpoint even when no limits have been observed yet.
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(500);

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

    fn note(&mut self) {
        self.sent.push_back(Instant::now());
    }

    /// True when a 429 told us to wait (so the loop must not invent its own
    /// backoff on top).
    fn is_restricted(&self) -> bool {
        self.restricted_until.is_some_and(|t| t > Instant::now())
    }

    /// How long we must wait before the next request is within every limit.
    fn delay(&mut self) -> Duration {
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

    fn observe(&mut self, resp: &ureq::http::Response<ureq::Body>) {
        let headers = resp.headers();
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
                // `active` (third field) means this rule is currently restricting us.
                if let Some(active) = parse_limits(&state)
                    .iter()
                    .map(|l| l.restrict)
                    .max()
                    .filter(|v| *v > 0)
                {
                    self.restricted_until = Some(Instant::now() + Duration::from_secs(active));
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
        let agent = ureq::Agent::config_builder()
            // We inspect status codes ourselves: 429 has meaning beyond "error".
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .new_agent();
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

    /// Fetch the page starting at `change_id` (or the stream head when `None`).
    /// Retries rate limiting and transient failures internally.
    pub fn fetch(&mut self, change_id: Option<&str>, realm: Option<&str>) -> Result<StashPage> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let wait = self.limiter.delay();
            if !wait.is_zero() {
                self.total_wait += wait;
                std::thread::sleep(wait);
            }

            let mut url = self.base.clone();
            let mut sep = '?';
            if let Some(id) = change_id {
                url.push(sep);
                url.push_str("id=");
                url.push_str(&urlencode(id));
                sep = '&';
            }
            if let Some(realm) = realm {
                url.push(sep);
                url.push_str("realm=");
                url.push_str(&urlencode(realm));
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
            self.limiter.observe(&resp);

            let status = resp.status().as_u16();
            match status {
                200 => {
                    let text = resp
                        .body_mut()
                        .read_to_string()
                        .context("reading public stash page body")?;
                    return StashPage::parse(text.as_bytes()).with_context(|| {
                        format!(
                            "parsing page ({} bytes, change_id={change_id:?})",
                            text.len()
                        )
                    });
                }
                401 => bail!(
                    "401 Unauthorized: the access token was rejected.\n\
                     Tokens can be revoked from the application settings page. Run `big_oil auth --force`."
                ),
                403 => bail!(
                    "403 Forbidden: {}\n\
                     The application needs the `service:psapi` scope (client_credentials grant).",
                    resp.body_mut().read_to_string().unwrap_or_default().trim()
                ),
                429 => {
                    if attempt >= MAX_ATTEMPTS {
                        bail!("429 Too Many Requests after {attempt} attempts; backing off");
                    }
                    // `observe` already set restricted_until from Retry-After;
                    // fall back to exponential backoff if the header was absent.
                    if !self.limiter.is_restricted() {
                        let backoff = Duration::from_secs(5 * attempt as u64);
                        self.total_wait += backoff;
                        std::thread::sleep(backoff);
                    }
                    continue;
                }
                500..=599 => {
                    if attempt >= MAX_ATTEMPTS {
                        bail!("server error {status} after {attempt} attempts");
                    }
                    let backoff = Duration::from_secs(1 << attempt.min(5));
                    self.total_wait += backoff;
                    tracing::warn!(status, ?backoff, "server error, retrying");
                    std::thread::sleep(backoff);
                    continue;
                }
                other => {
                    let body = resp.body_mut().read_to_string().unwrap_or_default();
                    bail!("unexpected HTTP {other}: {}", body.trim());
                }
            }
        }
    }
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
}
