//! Client behaviour against a local mock: request shape, headers, that a 429
//! with `Retry-After` is honoured before the retry, and the typed errors the
//! stream loop branches on.

mod support;

use big_oil::client::{BadPage, Client, HttpStatus, fetch_head};
use std::time::{Duration, Instant};
use support::{mock, response};

fn client(addr: &str) -> Client {
    Client::new(
        format!("{addr}/public-stash-tabs"),
        "t",
        "me@example.com",
        "myapp",
    )
    .with_min_interval(Duration::ZERO)
}

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
        req.starts_with("GET /public-stash-tabs/xbox?id=cursor-1 "),
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
    let err = client.fetch(None, None).unwrap_err();
    assert_eq!(err.downcast_ref::<HttpStatus>().map(|s| s.code), Some(403));
    assert!(err.to_string().contains("service:psapi"), "{err}");
    handle.join().unwrap();
}

#[test]
fn bad_request_is_typed() {
    let (addr, handle) = mock(vec![response("400 Bad Request", &[], "invalid id")]);
    let err = client(&addr).fetch(Some("stale"), None).unwrap_err();
    let status = err.downcast_ref::<HttpStatus>().expect("typed HTTP error");
    assert_eq!(status.code, 400);
    assert!(status.message.contains("rejected the cursor"), "{status}");
    handle.join().unwrap();
}

#[test]
fn malformed_page_reports_next_id() {
    let (addr, handle) = mock(vec![response(
        "200 OK",
        &[("Content-Type", "application/json")],
        r#"{"next_change_id":"n2","stashes":[{"id":1}]}"#,
    )]);
    let err = client(&addr).fetch(Some("1-2-3"), None).unwrap_err();
    let bad = err.downcast_ref::<BadPage>().expect("typed bad page");
    assert_eq!(bad.next_change_id.as_deref(), Some("n2"));
    assert_eq!(bad.change_id.as_deref(), Some("1-2-3"));
    assert!(bad.body.contains("\"stashes\""));
    handle.join().unwrap();
}

#[test]
fn fetch_head_reads_psapi_id() {
    let (addr, handle) = mock(vec![response(
        "200 OK",
        &[("Content-Type", "application/json")],
        r#"{"psapi":"1-2-3","forum":"9"}"#,
    )]);
    let id = fetch_head(&format!("{addr}/api/trade/data/change-ids"), "big_oil/test").unwrap();
    assert_eq!(id, "1-2-3");
    let requests = handle.join().unwrap();
    assert!(!requests[0].to_ascii_lowercase().contains("authorization"));
    assert!(requests[0].contains("user-agent: big_oil/test"));
}
