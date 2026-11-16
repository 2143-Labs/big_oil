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
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or_default();
    let now = crate::store::unix_now();
    let (status, content_type, body) = match path {
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4",
            metrics::render(store, errors.load(Ordering::Relaxed), started_at, now)?,
        ),
        "/healthz" => {
            let fresh = store
                .newest_hour()?
                .is_some_and(|hour| hour >= current_hour(now) - STALE_AFTER);
            if fresh {
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
