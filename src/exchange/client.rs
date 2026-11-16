//! HTTP access to the hourly digest. Unauthenticated: no cookie, no token.

use crate::exchange::page::Page;
use anyhow::{Context, Result, bail};
use std::time::Duration;

/// Total attempts for a retryable answer (`429`, `5xx`).
pub const ATTEMPTS: u32 = 3;

pub fn hour_url(base: &str, hour: i64) -> String {
    format!("{}/{}", base.trim_end_matches('/'), hour)
}

/// One hour's page, plus the exact bytes it arrived as (what gets archived and
/// hashed).
pub fn get_hour(base: &str, hour: i64, user_agent: &str) -> Result<(Page, Vec<u8>)> {
    let url = hour_url(base, hour);
    let failed = || format!("could not fetch currency-exchange {hour} from {url}");
    let mut attempt = 0;
    loop {
        attempt += 1;
        let response = crate::client::agent(Duration::from_secs(30))
            .get(&url)
            .header("User-Agent", user_agent)
            .header("Accept", "application/json")
            .call();
        let mut resp = match response {
            Ok(resp) => resp,
            // A TLS or connect hiccup is as transient as a 5xx, and during a
            // backfill a plain give-up would cost an hour of catch-up.
            Err(e) if attempt < ATTEMPTS => {
                let wait = 30 * u64::from(attempt);
                tracing::warn!(hour, error = %e, wait, "retrying currency-exchange request");
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
            Err(e) => return Err(e).with_context(failed),
        };
        let status = resp.status().as_u16();
        // A 60-request backfill will meet a throttle eventually; honour the
        // server's own backoff when it offers one, but never for longer than
        // five minutes — a bogus value must not stall the hour.
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|secs| secs.min(300));
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 200 {
            let page = serde_json::from_str(&body).with_context(|| {
                format!("currency-exchange {hour} returned an unparseable page")
            })?;
            return Ok((page, body.into_bytes()));
        }
        let excerpt: String = body.trim().chars().take(300).collect();
        if (status == 429 || status >= 500) && attempt < ATTEMPTS {
            let wait = retry_after.unwrap_or(30 * u64::from(attempt));
            tracing::warn!(hour, status, wait, "retrying currency-exchange request");
            std::thread::sleep(Duration::from_secs(wait));
            continue;
        }
        bail!("currency-exchange {hour} returned HTTP {status}: {excerpt}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_url_handles_trailing_slash() {
        assert_eq!(
            hour_url("http://x/api", 1794600000),
            "http://x/api/1794600000"
        );
        assert_eq!(
            hour_url("http://x/api/", 1794600000),
            "http://x/api/1794600000"
        );
    }
}
