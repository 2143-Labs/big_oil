//! Client behaviour against a local mock: request shape, headers, and that a
//! 429 with `Retry-After` is honoured before the retry.

mod support;

use big_oil::client::Client;
use std::time::{Duration, Instant};
use support::{mock, response};

const PAGE: &str = r#"{"next_change_id":"next","stashes":[
    {"id":"s1","public":true,"accountName":"acct#1","stash":"Shop","stashType":"PremiumStash",
     "league":"Standard","items":[{"id":"i1","name":"","typeLine":"Chaos Orb","frameType":5,
     "note":"~b/o 1 divine","stackSize":3}]}]}"#;

#[test]
fn sends_bearer_auth_and_paginates() {
    let (addr, handle) = mock(vec![response(
        "200 OK",
        &[
            ("Content-Type", "application/json"),
            ("X-Rate-Limit-Rules", "Ip"),
            ("X-Rate-Limit-Ip", "6:12:60"),
            ("X-Rate-Limit-Ip-State", "1:12:0"),
        ],
        PAGE,
    )]);

    let mut client = Client::new(
        format!("{addr}/public-stash-tabs"),
        "secret-token",
        "me@example.com",
        "myapp",
    )
    .with_min_interval(Duration::ZERO);

    let page = client.fetch(Some("cursor-1"), Some("xbox")).unwrap();
    assert_eq!(page.next_change_id, "next");
    assert_eq!(page.stashes.len(), 1);

    let requests = handle.join().unwrap();
    let req = &requests[0];
    assert!(
        req.starts_with("GET /public-stash-tabs?id=cursor-1&realm=xbox "),
        "{req}"
    );
    assert!(req.contains("authorization: Bearer secret-token"), "{req}");
    assert!(
        req.contains("user-agent: OAuth myapp/0.1.0 (contact: me@example.com)"),
        "{req}"
    );
}

#[test]
fn honours_retry_after_on_429() {
    let (addr, handle) = mock(vec![
        response(
            "429 Too Many Requests",
            &[
                ("Retry-After", "1"),
                ("X-Rate-Limit-Rules", "Ip"),
                ("X-Rate-Limit-Ip", "6:12:60"),
                // One hit (this request), so only Retry-After decides the
                // wait; a full window would add its own (tested in the unit
                // tests).
                ("X-Rate-Limit-Ip-State", "1:12:0"),
            ],
            "",
        ),
        response("200 OK", &[("Content-Type", "application/json")], PAGE),
    ]);

    let mut client = Client::new(
        format!("{addr}/public-stash-tabs"),
        "t",
        "me@example.com",
        "myapp",
    )
    .with_min_interval(Duration::ZERO);

    let started = Instant::now();
    let page = client.fetch(None, None).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(page.stashes.len(), 1);
    assert!(
        elapsed >= Duration::from_millis(900),
        "should have waited for Retry-After, took {elapsed:?}"
    );
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2, "429 must be retried once");
    assert!(requests[0].starts_with("GET /public-stash-tabs HTTP/1.1"));
}

#[test]
fn forbidden_reports_the_missing_scope() {
    let (addr, handle) = mock(vec![response(
        "403 Forbidden",
        &[("Content-Type", "application/json")],
        r#"{"error":{"code":6,"message":"Forbidden"}}"#,
    )]);
    let mut client = Client::new(
        format!("{addr}/public-stash-tabs"),
        "t",
        "me@example.com",
        "myapp",
    );
    let err = client.fetch(None, None).unwrap_err().to_string();
    assert!(err.contains("service:psapi"), "{err}");
    handle.join().unwrap();
}
