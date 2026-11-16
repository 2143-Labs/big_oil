//! The currency-exchange collector: cursor rules, completeness, and the
//! position-independence of the ratio dicts.

mod support;

use big_oil::exchange::page::Page;
use big_oil::exchange::report::verify;
use big_oil::exchange::store::ExchangeStore;
use big_oil::exchange::walk::walk;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use support::{mock, response};

/// 2026-11-13 20:00 UTC: the first hour of the Covens league.
const H: i64 = 1_794_600_000;
const UA: &str = "big_oil-test/0 (contact: test)";
const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";
const EXALT: &str = "Metadata/Items/Currency/CurrencyAddModToRare";

fn dict(pairs: &[(&str, i64)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), Value::from(*v));
    }
    Value::Object(map)
}

/// The value dicts are keyed by item, so they describe the same market whatever
/// order `pair` is given in — reversing the pair must only reverse the pair.
fn market(league: &str, pair: [&str; 2]) -> Value {
    serde_json::json!({
        "league": league,
        "market_id": format!("{}|{}", pair[0], pair[1]),
        "market_pair": pair,
        "volume_traded": dict(&[(CHAOS, 4), (DIVINE, 1), (EXALT, 2)]),
        "lowest_stock": dict(&[(CHAOS, 10)]),
        "highest_stock": dict(&[(CHAOS, 20)]),
        "lowest_ratio": dict(&[(CHAOS, 850), (DIVINE, 1), (EXALT, 300)]),
        "highest_ratio": dict(&[(CHAOS, 860), (DIVINE, 1), (EXALT, 300)]),
    })
}

fn body(next_change_id: i64, markets: Vec<Value>) -> String {
    response(
        "200 OK",
        &[("Content-Type", "application/json")],
        &serde_json::json!({
            "next_change_id": next_change_id,
            "markets": markets,
        })
        .to_string(),
    )
}

fn raw_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("big_oil_exchange_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn stores_hours_and_proves_completeness() {
    let (base, server) = mock(vec![
        body(
            H + 3600,
            vec![
                market("Covens", [CHAOS, DIVINE]),
                market("Standard", [CHAOS, EXALT]),
            ],
        ),
        body(H + 7200, vec![market("Covens", [DIVINE, EXALT])]),
        // An hour nobody traded: still an hour.
        body(H + 10800, vec![]),
    ]);
    let mut store = ExchangeStore::open_memory().unwrap();
    let raw = raw_dir("complete");

    let walked = walk(&mut store, &base, UA, H, Duration::ZERO, &raw, H + 3 * 3600).unwrap();
    assert_eq!(walked.stored, vec![H, H + 3600, H + 7200]);
    assert_eq!(walked.cursor, H + 3 * 3600);
    assert_eq!(store.hour_count().unwrap(), 3);
    assert_eq!(store.count_for_hour(H).unwrap(), 2);
    assert_eq!(store.count_for_hour(H + 7200).unwrap(), 0);

    let report = verify(&store, Some(H), false, &base, UA, Duration::ZERO).unwrap();
    assert!(report.is_clean(), "{}", report.describe());
    assert_eq!(report.hours, 3);
    assert_eq!(report.first, Some(H));

    // One gzipped payload per hour, and the requests were addressed by hour id.
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    for (i, request) in requests.iter().enumerate() {
        let hour = H + i as i64 * 3600;
        assert!(request.starts_with(&format!("GET /{hour} ")), "{request}");
    }
    assert_eq!(std::fs::read_dir(&raw).unwrap().count(), 3);
}

#[test]
fn cursor_jump_is_an_error() {
    // Two hours ahead instead of one: this would skip an hour silently.
    let (base, server) = mock(vec![body(
        H + 7200,
        vec![market("Covens", [CHAOS, DIVINE])],
    )]);
    let mut store = ExchangeStore::open_memory().unwrap();

    let err = walk(
        &mut store,
        &base,
        UA,
        H,
        Duration::ZERO,
        &raw_dir("jump"),
        H + 3 * 3600,
    )
    .unwrap_err();
    assert!(err.to_string().contains("next_change_id"), "{err}");
    assert_eq!(store.hour_count().unwrap(), 0);
    let _ = server.join().unwrap();
}

#[test]
fn malformed_pair_is_skipped() {
    let mut broken = market("Covens", [CHAOS, DIVINE]);
    broken["market_pair"] = serde_json::json!([CHAOS]);
    let (base, server) = mock(vec![body(
        H + 3600,
        vec![broken, market("Covens", [CHAOS, EXALT])],
    )]);
    let mut store = ExchangeStore::open_memory().unwrap();

    let walked = walk(
        &mut store,
        &base,
        UA,
        H,
        Duration::ZERO,
        &raw_dir("malformed"),
        H + 3600,
    )
    .unwrap();
    assert_eq!(walked.stored, vec![H]);
    assert_eq!(store.count_for_hour(H).unwrap(), 1);
    let _ = server.join().unwrap();
}

#[test]
fn canonical_order_is_position_independent() {
    let mut store = ExchangeStore::open_memory().unwrap();
    for (i, pair) in [[CHAOS, DIVINE], [DIVINE, CHAOS]].into_iter().enumerate() {
        let hour = H + i as i64 * 3600;
        let page: Page = serde_json::from_value(serde_json::json!({
            "next_change_id": hour + 3600,
            "markets": [market("Covens", pair)],
        }))
        .unwrap();
        store.record_hour(hour, &page, 1, "hash").unwrap();
    }
    let first = store.markets_at(H).unwrap();
    let second = store.markets_at(H + 3600).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].item, DIVINE);
    assert!((first[0].chaos_mid - 855.0).abs() < 1e-9);
    assert_eq!(first[0].chaos_mid, second[0].chaos_mid);
    assert_eq!(first[0].volume, second[0].volume);
}

#[test]
fn empty_hour_is_still_recorded() {
    let (base, server) = mock(vec![
        body(H + 3600, vec![market("Covens", [CHAOS, DIVINE])]),
        body(H + 7200, vec![]),
    ]);
    let mut store = ExchangeStore::open_memory().unwrap();

    walk(
        &mut store,
        &base,
        UA,
        H,
        Duration::ZERO,
        &raw_dir("empty"),
        H + 2 * 3600,
    )
    .unwrap();
    assert_eq!(store.hour_count().unwrap(), 2);
    assert_eq!(store.count_for_hour(H + 3600).unwrap(), 0);
    let report = verify(&store, Some(H), false, &base, UA, Duration::ZERO).unwrap();
    assert!(report.is_clean(), "{}", report.describe());
    let _ = server.join().unwrap();
}

#[test]
fn resumes_from_the_newest_hour() {
    let (base, server) = mock(vec![
        body(H + 3600, vec![market("Covens", [CHAOS, DIVINE])]),
        body(H + 7200, vec![market("Covens", [CHAOS, EXALT])]),
    ]);
    let mut store = ExchangeStore::open_memory().unwrap();
    let raw = raw_dir("resume");

    walk(&mut store, &base, UA, H, Duration::ZERO, &raw, H + 3600).unwrap();
    let newest = store.newest_hour().unwrap().unwrap();
    assert_eq!(newest, H);

    let second = walk(
        &mut store,
        &base,
        UA,
        newest + 3600,
        Duration::ZERO,
        &raw,
        H + 2 * 3600,
    )
    .unwrap();
    assert_eq!(second.stored, vec![H + 3600]);

    // Exactly two requests: stored hours are never asked for again.
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with(&format!("GET /{} ", H + 3600)));
}
