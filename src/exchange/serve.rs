//! The long-running mode: collect an hour, sleep until the next boundary, and
//! serve `/metrics`, `/healthz` and the read-only `/v1` price API meanwhile.
//!
//! The walk runs on its own OS thread with the only writing connection. HTTP
//! handlers share one read-only connection: `rusqlite::Connection` is `Send`
//! but not `Sync`, so it sits behind a mutex that is only ever taken on the
//! blocking pool, never across an `.await`.

use crate::exchange::HOUR;
use crate::exchange::metrics;
use crate::exchange::store::ExchangeStore;
use crate::exchange::walk::{current_hour, walk};
use anyhow::{Context, Result, bail};
use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Extra wait past the hour boundary before asking for the hour that just
/// closed: GGG needs a moment to publish it.
const BOUNDARY_GRACE: i64 = 120;

/// The hours of silence after which `/healthz` reports stale.
const STALE_AFTER: i64 = 3 * HOUR;

/// How recently an hour must have been written for a catch-up to count as
/// healthy. One hour takes about two seconds, so this is a generous margin.
const PROGRESS_WINDOW: i64 = 900;

/// `/v1/prices` default and maximum window, in stored hours.
const DEFAULT_HOURS: u32 = 24;
const MAX_HOURS: u32 = 168;

/// The league `/v1/prices` answers for when none is asked for.
const DEFAULT_LEAGUE: &str = "Covens";

/// The data changes once an hour; a minute of caching absorbs bursts.
const API_CACHE: &str = "public, max-age=60";

pub fn run(
    db: PathBuf,
    base: String,
    user_agent: String,
    since: Option<i64>,
    listen: SocketAddr,
    pace: Duration,
) -> Result<()> {
    let writer = ExchangeStore::open(&db)?;
    // Bind before doing any work: a port that cannot be taken must fail now,
    // not after a backfill.
    let listener =
        std::net::TcpListener::bind(listen).with_context(|| format!("binding {listen}"))?;
    listener.set_nonblocking(true)?;
    // Opened after the writer, which has created the file and its WAL files.
    let reader = ExchangeStore::open_readonly(&db)?;

    let start = match writer.newest_hour()? {
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
    let raw_dir = db.with_file_name("exchange-raw");

    let errors = Arc::new(AtomicU64::new(0));
    let started_at = crate::store::unix_now();
    {
        let errors = Arc::clone(&errors);
        std::thread::spawn(move || {
            walk_loop(writer, base, user_agent, start, pace, raw_dir, errors)
        });
    }
    let app = Arc::new(App {
        store: Mutex::new(reader),
        errors,
        started_at,
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        axum::serve(listener, router(app))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        anyhow::Ok(())
    })
}

/// Walk to the live edge, sleep to the next boundary, repeat. Never returns: a
/// SIGTERM ends the process, and a half-written hour rolls back and is fetched
/// again on the next start (hours are idempotent).
fn walk_loop(
    mut store: ExchangeStore,
    base: String,
    user_agent: String,
    mut start: i64,
    pace: Duration,
    raw_dir: PathBuf,
    errors: Arc<AtomicU64>,
) -> ! {
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

async fn shutdown_signal() {
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}

struct App {
    store: Mutex<ExchangeStore>,
    errors: Arc<AtomicU64>,
    started_at: i64,
}

type Shared = Arc<App>;

fn router(app: Shared) -> Router {
    Router::new()
        .route("/metrics", get(metrics_route))
        .route("/healthz", get(healthz_route))
        .route("/v1/leagues", get(leagues_route))
        .route("/v1/prices", get(prices_route))
        .fallback(not_found)
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(app)
}

/// Run `f` against the shared reader on the blocking pool: SQLite calls are
/// synchronous, and the mutex must never be held by an async worker.
async fn with_store<F>(app: Shared, f: F) -> Response
where
    F: FnOnce(&App, &ExchangeStore) -> Result<Response> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        let store = app.store.lock().unwrap_or_else(PoisonError::into_inner);
        f(&app, &store)
    })
    .await;
    match joined {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "request failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
        Err(e) => {
            tracing::warn!(error = %e, "request handler panicked");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

async fn metrics_route(State(app): State<Shared>) -> Response {
    with_store(app, |app, store| {
        let body = metrics::render(
            store,
            app.errors.load(Ordering::Relaxed),
            app.started_at,
            crate::store::unix_now(),
        )?;
        Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response())
    })
    .await
}

async fn healthz_route(State(app): State<Shared>) -> Response {
    with_store(app, |_, store| {
        let now = crate::store::unix_now();
        let fresh = store
            .newest_hour()?
            .is_some_and(|hour| hour >= current_hour(now) - STALE_AFTER);
        // A catch-up writes one hour every few seconds while it is behind the
        // live edge, so a recent write also means healthy: otherwise a backfill
        // of hundreds of hours would trip the liveness probe.
        let writing = store
            .newest_fetch_at()?
            .is_some_and(|at| now - at < PROGRESS_WINDOW);
        Ok(if fresh || writing {
            (StatusCode::OK, "ok\n").into_response()
        } else {
            (StatusCode::SERVICE_UNAVAILABLE, "stale\n").into_response()
        })
    })
    .await
}

async fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "not found")
}

#[derive(serde::Serialize)]
struct Point {
    hour: i64,
    chaos: f64,
    low: f64,
    high: f64,
    volume: i64,
}

#[derive(serde::Serialize)]
struct ItemSeries {
    item: String,
    points: Vec<Point>,
}

#[derive(serde::Serialize)]
struct PricesResponse {
    league: String,
    generated_at: i64,
    newest_hour: i64,
    from: i64,
    to: i64,
    hours: u32,
    items: Vec<ItemSeries>,
}

#[derive(serde::Serialize)]
struct LeagueRow {
    league: String,
    markets: i64,
}

#[derive(serde::Serialize)]
struct LeaguesResponse {
    generated_at: i64,
    newest_hour: i64,
    leagues: Vec<LeagueRow>,
}

#[derive(serde::Deserialize)]
struct PricesQuery {
    league: Option<String>,
    hours: Option<u32>,
    item: Option<String>,
}

async fn leagues_route(State(app): State<Shared>) -> Response {
    with_store(app, |_, store| {
        let Some(newest) = store.newest_hour()? else {
            return Ok(json_error(StatusCode::SERVICE_UNAVAILABLE, "no data"));
        };
        let leagues = public_leagues(store, newest)?
            .into_iter()
            .map(|(league, markets)| LeagueRow { league, markets })
            .collect();
        Ok(api_json(LeaguesResponse {
            generated_at: crate::store::unix_now(),
            newest_hour: newest,
            leagues,
        }))
    })
    .await
}

async fn prices_route(
    State(app): State<Shared>,
    query: Result<Query<PricesQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return json_error(StatusCode::BAD_REQUEST, "invalid query");
    };
    let hours = query.hours.unwrap_or(DEFAULT_HOURS);
    if hours == 0 || hours > MAX_HOURS {
        return json_error(StatusCode::BAD_REQUEST, "hours must be 1..=168");
    }
    with_store(app, move |_, store| prices(store, query, hours)).await
}

fn prices(store: &ExchangeStore, query: PricesQuery, hours: u32) -> Result<Response> {
    let Some(newest) = store.newest_hour()? else {
        return Ok(json_error(StatusCode::SERVICE_UNAVAILABLE, "no data"));
    };
    let leagues = public_leagues(store, newest)?;
    let league = match query.league {
        // A private league is answered exactly like a missing one, so its name
        // cannot be probed for.
        Some(name) => {
            if !leagues.iter().any(|(l, _)| *l == name) {
                return Ok(json_error(StatusCode::NOT_FOUND, "unknown league"));
            }
            name
        }
        None => match leagues
            .iter()
            .find(|(l, _)| l == DEFAULT_LEAGUE)
            .or(leagues.first())
        {
            Some((l, _)) => l.clone(),
            None => return Ok(json_error(StatusCode::SERVICE_UNAVAILABLE, "no data")),
        },
    };
    let from = newest - (i64::from(hours) - 1) * HOUR;
    let mut series: BTreeMap<String, Vec<Point>> = BTreeMap::new();
    for row in store.prices_range(&league, from, newest)? {
        if query.item.as_ref().is_some_and(|item| *item != row.item) {
            continue;
        }
        // Rows arrive newest hour first, so each series does too.
        series.entry(row.item).or_default().push(Point {
            hour: row.hour,
            chaos: round6(row.chaos_mid),
            low: round6(row.chaos_low),
            high: round6(row.chaos_high),
            volume: row.volume,
        });
    }
    Ok(api_json(PricesResponse {
        league,
        generated_at: crate::store::unix_now(),
        newest_hour: newest,
        from,
        to: newest,
        hours,
        items: series
            .into_iter()
            .map(|(item, points)| ItemSeries { item, points })
            .collect(),
    }))
}

/// The newest hour's leagues, busiest first, without private ones.
fn public_leagues(store: &ExchangeStore, hour: i64) -> Result<Vec<(String, i64)>> {
    Ok(store
        .leagues_at(hour)?
        .into_iter()
        .filter(|(name, _)| is_public_league(name))
        .collect())
}

/// GGG names private leagues `<name> (PL<digits>)`. Those are players' own
/// league names and are never published.
fn is_public_league(name: &str) -> bool {
    let Some((_, rest)) = name.rsplit_once(" (PL") else {
        return true;
    };
    let Some(digits) = rest.strip_suffix(')') else {
        return true;
    };
    digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn api_json<T: serde::Serialize>(body: T) -> Response {
    ([(header::CACHE_CONTROL, API_CACHE)], Json(body)).into_response()
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
    const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";
    const EXALT: &str = "Metadata/Items/Currency/CurrencyAddModToRare";

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

    /// One chaos market: `item` priced between `chaos_lo` and `chaos_hi` chaos,
    /// with `volume` of the item traded.
    fn market(
        league: &str,
        item: &str,
        chaos_lo: i64,
        chaos_hi: i64,
        volume: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "league": league,
            "market_id": format!("{CHAOS}|{item}"),
            "market_pair": [CHAOS, item],
            "volume_traded": json_dict(&[(CHAOS, volume * chaos_lo), (item, volume)]),
            "lowest_ratio": json_dict(&[(CHAOS, chaos_lo), (item, 1)]),
            "highest_ratio": json_dict(&[(CHAOS, chaos_hi), (item, 1)]),
        })
    }

    fn record(store: &mut ExchangeStore, hour: i64, markets: Vec<serde_json::Value>) {
        let page = serde_json::from_value(serde_json::json!({
            "next_change_id": hour + HOUR,
            "markets": markets,
        }))
        .unwrap();
        store.record_hour(hour, &page, 1, "hash").unwrap();
    }

    fn store_with_one_hour() -> ExchangeStore {
        let mut store = ExchangeStore::open_memory().unwrap();
        let page = serde_json::from_value(serde_json::json!({
            "next_change_id": HOUR,
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

    /// `n` consecutive hours of one Covens chaos/divine market, ending at
    /// `(n - 1) * HOUR`.
    fn store_with_hours(n: i64) -> ExchangeStore {
        let mut store = ExchangeStore::open_memory().unwrap();
        for h in 0..n {
            record(
                &mut store,
                h * HOUR,
                vec![market("Covens", DIVINE, 150, 170, 10)],
            );
        }
        store
    }

    async fn get(store: ExchangeStore, uri: &str) -> (StatusCode, Response) {
        let app = Arc::new(App {
            store: Mutex::new(store),
            errors: Arc::new(AtomicU64::new(0)),
            started_at: 0,
        });
        let response = router(app)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        (response.status(), response)
    }

    async fn body(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    async fn get_json(store: ExchangeStore, uri: &str) -> (StatusCode, serde_json::Value) {
        let (status, response) = get(store, uri).await;
        let bytes = body(response).await;
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn prices_defaults_to_24_newest_hours() {
        let (status, json) = get_json(store_with_hours(30), "/v1/prices").await;
        assert_eq!(status, StatusCode::OK);
        let newest = 29 * HOUR;
        assert_eq!(json["league"], "Covens");
        assert_eq!(json["hours"], 24);
        assert_eq!(json["newest_hour"], newest);
        assert_eq!(json["from"], newest - 23 * HOUR);
        assert_eq!(json["to"], newest);
        let points = json["items"][0]["points"].as_array().unwrap();
        assert_eq!(points.len(), 24);
        assert_eq!(points[0]["hour"], newest);
        assert_eq!(points[23]["hour"], newest - 23 * HOUR);
    }

    #[tokio::test]
    async fn prices_carries_low_mid_high_and_volume() {
        let mut store = ExchangeStore::open_memory().unwrap();
        record(&mut store, 0, vec![market("Covens", DIVINE, 160, 170, 35)]);
        let (status, json) = get_json(store, "/v1/prices?hours=1").await;
        assert_eq!(status, StatusCode::OK);
        let point = &json["items"][0]["points"][0];
        assert_eq!(json["items"][0]["item"], DIVINE);
        assert_eq!(point["low"], 160.0);
        assert_eq!(point["chaos"], 165.0);
        assert_eq!(point["high"], 170.0);
        assert_eq!(point["volume"], 35);
    }

    #[tokio::test]
    async fn prices_rejects_hours_outside_the_range() {
        for uri in [
            "/v1/prices?hours=0",
            "/v1/prices?hours=169",
            "/v1/prices?hours=abc",
        ] {
            let (status, json) = get_json(store_with_hours(2), uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert!(json["error"].is_string(), "{uri}: {json}");
        }
    }

    #[tokio::test]
    async fn prices_hides_private_leagues() {
        let mut store = ExchangeStore::open_memory().unwrap();
        record(
            &mut store,
            0,
            vec![
                market("Covens", DIVINE, 160, 170, 35),
                market("Test (PL123)", DIVINE, 160, 170, 35),
            ],
        );
        let (status, json) = get_json(store, "/v1/prices?league=Test%20(PL123)").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "unknown league");
        let (status, _) = get_json(store_with_hours(1), "/v1/prices?league=Nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn store_with_two_items() -> ExchangeStore {
        let mut store = ExchangeStore::open_memory().unwrap();
        record(
            &mut store,
            0,
            vec![
                market("Covens", DIVINE, 160, 170, 35),
                market("Covens", EXALT, 10, 12, 400),
            ],
        );
        store
    }

    #[tokio::test]
    async fn prices_filters_to_one_item() {
        let (_, all) = get_json(store_with_two_items(), "/v1/prices").await;
        assert_eq!(all["items"].as_array().unwrap().len(), 2);

        let uri = format!("/v1/prices?item={EXALT}");
        let (status, one) = get_json(store_with_two_items(), &uri).await;
        assert_eq!(status, StatusCode::OK);
        let items = one["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["item"], EXALT);

        let (status, none) = get_json(store_with_two_items(), "/v1/prices?item=Nope").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(none["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn api_responses_are_cacheable_for_a_minute() {
        let (_, response) = get(store_with_hours(1), "/v1/leagues").await;
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=60"
        );
    }

    #[tokio::test]
    async fn leagues_excludes_private_leagues() {
        let mut store = ExchangeStore::open_memory().unwrap();
        record(
            &mut store,
            0,
            vec![
                market("Covens", DIVINE, 160, 170, 35),
                market("Puzzle (PL1)", DIVINE, 160, 170, 35),
            ],
        );
        let (status, json) = get_json(store, "/v1/leagues").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["leagues"],
            serde_json::json!([{"league": "Covens", "markets": 1}])
        );
    }

    #[test]
    fn private_league_names_are_recognised() {
        for name in [
            "Progress and Poverty (PL87708)",
            "Poverty League Bargain (PL88132)",
            "No scarabs oops (PL87702)",
            "Puzzle (PL1)",
        ] {
            assert!(!is_public_league(name), "{name}");
        }
        for name in [
            "Covens",
            "Hardcore Covens",
            "HC Ruthless Covens",
            "Standard",
            "Phrecia 2.0",
            "Odd (PL)",
            "Odd (PLx1)",
        ] {
            assert!(is_public_league(name), "{name}");
        }
    }

    #[tokio::test]
    async fn metrics_are_still_served() {
        let (status, response) = get(store_with_one_hour(), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
        let text = String::from_utf8(body(response).await).unwrap();
        assert!(text.contains("poe_exchange_hours 1"), "{text}");
    }

    #[tokio::test]
    async fn healthz_is_ok_after_a_recent_write() {
        // A catch-up that is still behind the live edge is healthy as long as it
        // keeps writing, which is what keeps the liveness probe from restarting
        // the pod mid-backfill.
        let (status, response) = get(store_with_one_hour(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body(response).await, b"ok\n");
    }

    #[tokio::test]
    async fn healthz_is_stale_before_anything_is_stored() {
        let (status, response) = get(ExchangeStore::open_memory().unwrap(), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body(response).await, b"stale\n");
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        let (status, json) = get_json(store_with_one_hour(), "/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "not found");
    }
}
