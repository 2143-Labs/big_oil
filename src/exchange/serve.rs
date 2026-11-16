//! The long-running mode: collect an hour, sleep until the next boundary, serve
//! `/metrics` and `/healthz` in between.

use crate::exchange::HOUR;
use crate::exchange::metrics;
use crate::exchange::store::ExchangeStore;
use crate::exchange::walk::{current_hour, walk};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Extra wait past the hour boundary before asking for the hour that just
/// closed: GGG needs a moment to publish it.
const BOUNDARY_GRACE: i64 = 120;

/// The hours of silence after which `/healthz` reports stale.
const STALE_AFTER: i64 = 3 * HOUR;

/// How recently an hour must have been written for a catch-up to count as
/// healthy. One hour takes about two seconds, so this is a generous margin.
const PROGRESS_WINDOW: i64 = 900;

pub fn run(
    db: PathBuf,
    base: String,
    user_agent: String,
    since: Option<i64>,
    listen: SocketAddr,
    pace: Duration,
) -> Result<()> {
    let mut store = ExchangeStore::open(&db)?;
    // Bind before doing any work: a port that cannot be taken must fail now,
    // not after a backfill.
    let listener = TcpListener::bind(listen).with_context(|| format!("binding {listen}"))?;
    let raw_dir = db.with_file_name("exchange-raw");

    let mut start = match store.newest_hour()? {
        Some(newest) => {
            if since.is_some() {
                tracing::info!(newest, "ignoring --since: the database already holds hours");
            }
            newest + HOUR
        }
        None => match since {
            Some(since) => since,
            None => bail!(
                "the exchange database is empty: pass --since for the first run \
                 (the Covens league started at 1794600000)"
            ),
        },
    };

    let errors = Arc::new(AtomicU64::new(0));
    let started_at = crate::store::unix_now();
    {
        let served = ExchangeStore::open(&db)?;
        let errors = Arc::clone(&errors);
        std::thread::spawn(move || serve(&listener, &served, &errors, started_at));
    }

    loop {
        match walk(
            &mut store,
            &base,
            &user_agent,
            start,
            pace,
            &raw_dir,
            crate::store::unix_now(),
        ) {
            Ok(walked) => {
                if !walked.stored.is_empty() {
                    tracing::info!(
                        hours = walked.stored.len(),
                        cursor = walked.cursor,
                        "exchange walk finished"
                    );
                }
                start = walked.cursor;
            }
            Err(e) => {
                // Never exit: the hour that failed becomes available again on
                // the next boundary, and the pod must stay scrapeable.
                errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %format!("{e:#}"), "exchange walk failed");
            }
        }
        let now = crate::store::unix_now();
        let next = current_hour(now) + HOUR + BOUNDARY_GRACE;
        let wait = (next - now).max(1) as u64;
        tracing::info!(seconds = wait, "sleeping until the next hour");
        std::thread::sleep(Duration::from_secs(wait));
    }
}

fn serve(listener: &TcpListener, store: &ExchangeStore, errors: &AtomicU64, started_at: i64) {
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(e) = handle(&mut stream, store, errors, started_at) {
                    tracing::debug!(error = %e, "dropping a scrape connection");
                }
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
}

fn handle(
    stream: &mut TcpStream,
    store: &ExchangeStore,
    errors: &AtomicU64,
    started_at: i64,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // Read up to the end of the request head. One `read` is not enough: behind a
    // proxy or NAT the request can arrive in several segments, and closing a
    // socket with unread bytes makes the kernel answer RST instead of FIN — the
    // client then reports a reset and discards the response it did get. The
    // deadline bounds the whole read, because the socket timeout above only
    // bounds each `read`: a client trickling one byte at a time would otherwise
    // hold the single serving thread and take the liveness probe down with it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while buf.len() < 8192 {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let now = crate::store::unix_now();
    let (status, content_type, body) = match path.as_str() {
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4",
            metrics::render(store, errors.load(Ordering::Relaxed), started_at, now)?,
        ),
        "/healthz" => {
            let fresh = store
                .newest_hour()?
                .is_some_and(|hour| hour >= current_hour(now) - STALE_AFTER);
            // A catch-up writes one hour every few seconds while it is behind
            // the live edge, so a recent write also means healthy: otherwise a
            // backfill of hundreds of hours would trip the liveness probe.
            let writing = store
                .newest_fetch_at()?
                .is_some_and(|at| now - at < PROGRESS_WINDOW);
            if fresh || writing {
                ("200 OK", "text/plain", "ok\n".to_string())
            } else {
                (
                    "503 Service Unavailable",
                    "text/plain",
                    "stale\n".to_string(),
                )
            }
        }
        _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
    const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";

    /// Serves exactly one request through `handle`, returning the response text.
    fn one_request<S>(store: ExchangeStore, send: S) -> String
    where
        S: FnOnce(&mut TcpStream),
    {
        let errors = AtomicU64::new(0);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handle(&mut stream, &store, &errors, 0).unwrap();
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        send(&mut client);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        response
    }

    fn store_with_one_hour() -> ExchangeStore {
        let mut store = ExchangeStore::open_memory().unwrap();
        let page = serde_json::from_value(serde_json::json!({
            "next_change_id": crate::exchange::HOUR,
            "markets": [{
                "league": "Covens",
                "market_id": format!("{CHAOS}|{DIVINE}"),
                "market_pair": [CHAOS, DIVINE],
                "volume_traded": dict(),
                "lowest_ratio": json_dict(&[(CHAOS, 850), (DIVINE, 1)]),
                "highest_ratio": json_dict(&[(CHAOS, 850), (DIVINE, 1)]),
            }]
        }))
        .unwrap();
        store.record_hour(0, &page, 1, "hash").unwrap();
        store
    }

    fn dict() -> serde_json::Value {
        serde_json::Value::Object(serde_json::Map::new())
    }

    fn json_dict(pairs: &[(&str, i64)]) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), serde_json::Value::from(*v));
        }
        serde_json::Value::Object(map)
    }

    #[test]
    fn answers_a_request_that_arrives_in_pieces() {
        // One read() would leave the tail unread, and closing that socket sends
        // RST, which the client reports as a reset.
        let response = one_request(store_with_one_hour(), |client| {
            client.write_all(b"GET /met").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            client
                .write_all(b"rics HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
        });
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("poe_exchange_hours 1"), "{response}");
    }

    #[test]
    fn healthz_is_ok_after_a_recent_write() {
        // A catch-up that is still behind the live edge is healthy as long as it
        // keeps writing, which is what keeps the liveness probe from restarting
        // the pod mid-backfill.
        let response = one_request(store_with_one_hour(), |client| {
            client
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
        });
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok\n"), "{response}");
    }

    #[test]
    fn healthz_is_stale_before_anything_is_stored() {
        let response = one_request(ExchangeStore::open_memory().unwrap(), |client| {
            client
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
        });
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{response}"
        );
        assert!(response.ends_with("stale\n"), "{response}");
    }

    #[test]
    fn unknown_paths_are_404() {
        let response = one_request(store_with_one_hour(), |client| {
            client
                .write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
        });
        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
    }
}
