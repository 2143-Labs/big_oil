//! Own-stash ingestion, valuation, and schema migration.

use big_oil::model::Item;
use big_oil::ninja::PriceLine;
use big_oil::own::OwnClient;
use big_oil::store::{SearchQuery, Store, unix_now};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn item(value: serde_json::Value) -> Item {
    Item::from_value(&value)
}

fn headhunter() -> Item {
    item(json!({
        "id": "h1", "name": "Headhunter", "typeLine": "Leather Belt", "baseType": "Leather Belt",
        "frameType": 3, "rarity": "Unique", "ilvl": 85, "corrupted": false
    }))
}

fn chaos_orb() -> Item {
    item(json!({
        "id": "c1", "name": "", "typeLine": "Chaos Orb", "baseType": "Chaos Orb",
        "frameType": 5, "stackSize": 312
    }))
}

fn line(name: &str, base: Option<&str>, chaos: f64) -> PriceLine {
    PriceLine {
        league: "Standard".into(),
        category: "Test".into(),
        name: Some(name.into()),
        base_type: base.map(str::to_owned),
        variant: None,
        links: None,
        corrupted: Some(false),
        gem_level: None,
        gem_quality: None,
        map_tier: None,
        chaos,
        divine: Some(chaos / 600.0),
        listing_count: Some(10),
        details_id: None,
        fetched_at: unix_now(),
    }
}

#[test]
fn own_tab_diff_records_drops_and_removals() {
    let mut store = Store::open_memory().unwrap();
    let stats = store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[headhunter(), chaos_orb()],
        )
        .unwrap();
    assert_eq!(stats.items_new, 2);

    let events: Vec<String> = store
        .history("h1")
        .unwrap()
        .into_iter()
        .map(|h| h.event)
        .collect();
    assert_eq!(events, vec!["dropped"]);

    // Move the belt away, drop something else in.
    let stats = store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[chaos_orb(), headhunter()],
        )
        .unwrap();
    assert_eq!(stats.items_new, 0, "same items are not re-listed");
    assert_eq!(stats.delisted, 0);

    let stats = store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[chaos_orb()],
        )
        .unwrap();
    assert_eq!(stats.delisted, 1);
    assert_eq!(
        store.history("h1").unwrap().last().unwrap().event,
        "removed"
    );

    let live = store
        .search(&SearchQuery {
            source: Some("own".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, "c1");
    assert_eq!(live[0].source, "own");

    let all = store
        .search(&SearchQuery {
            source: Some("own".into()),
            include_delisted: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn values_own_items_from_price_lines() {
    let mut store = Store::open_memory().unwrap();
    store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[headhunter(), chaos_orb()],
        )
        .unwrap();

    let lines = vec![
        line("Headhunter", Some("Leather Belt"), 6675.0),
        line("Chaos Orb", None, 1.0),
    ];
    let stored = store
        .upsert_price_lines("Standard", "Test", None, &lines)
        .unwrap();
    assert_eq!(stored, 2);

    let valued = store.value_items("Standard").unwrap();
    assert_eq!(valued, 2);

    let belt = store.item("h1").unwrap().unwrap();
    assert_eq!(belt.value_chaos, Some(6675.0));
    assert!(belt.valued_at.is_some());

    let history = store.valuations("h1").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].chaos, Some(6675.0));

    // A second pass appends history rather than replacing it.
    store.value_items("Standard").unwrap();
    assert_eq!(store.valuations("h1").unwrap().len(), 2);
}

#[test]
fn tabs_summary_totals_live_values() {
    let mut store = Store::open_memory().unwrap();
    store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[headhunter(), chaos_orb()],
        )
        .unwrap();
    store
        .upsert_price_lines(
            "Standard",
            "Test",
            None,
            &[
                line("Headhunter", Some("Leather Belt"), 6675.0),
                line("Chaos Orb", None, 1.0),
            ],
        )
        .unwrap();
    store.value_items("Standard").unwrap();

    let tabs = store.own_tabs().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].name, "Dump");
    assert_eq!(tabs[0].items, 2);
    assert_eq!(tabs[0].live, 2);
    assert_eq!(tabs[0].value_chaos, 6676.0);
}

#[test]
fn opens_a_v1_database_and_migrates_it() {
    let path = std::env::temp_dir().join(format!(
        "big_oil_migrate_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE stashes (id TEXT PRIMARY KEY, account TEXT, name TEXT, stash_type TEXT,
                 league TEXT, public INTEGER NOT NULL DEFAULT 1, first_seen INTEGER NOT NULL,
                 last_seen INTEGER NOT NULL, delisted_at INTEGER);
             CREATE TABLE items (id TEXT PRIMARY KEY, stash_id TEXT NOT NULL, league TEXT,
                 account TEXT, name TEXT, type_line TEXT, base_type TEXT, rarity TEXT,
                 frame_type INTEGER, ilvl INTEGER, stack_size INTEGER, sockets TEXT, links INTEGER,
                 corrupted INTEGER NOT NULL DEFAULT 0, identified INTEGER, icon TEXT, note TEXT,
                 price_amount REAL, price_currency TEXT, price_type TEXT, mods_text TEXT,
                 first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, delisted_at INTEGER,
                 raw TEXT NOT NULL);",
        )
        .unwrap();
    }

    let mut store = Store::open(&path).unwrap();
    let stats = store
        .ingest_own_tab(
            "stash-1",
            "Dump",
            "PremiumStash",
            "Standard",
            Some("Tester#1"),
            &[headhunter()],
        )
        .unwrap();
    assert_eq!(stats.items_new, 1);
    let row = store.item("h1").unwrap().unwrap();
    assert_eq!(row.source, "own");
    assert_eq!(row.value_chaos, None);
    std::fs::remove_file(&path).ok();
}

// --- mock endpoint for the own-stash client ---

fn response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = [0u8; 4096];
    let mut req = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&req).into_owned()
}

#[test]
fn client_sends_session_cookie_and_tab_params() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());

    let tabs_body = json!({
        "numTabs": 2,
        "tabs": [
            {"n": "Dump", "i": 0, "id": "aaaaaaaa-0000-0000-0000-000000000000",
             "type": "PremiumStash", "hidden": false},
            {"n": "Hideout", "i": 1, "id": "bbbbbbbb-0000-0000-0000-000000000000",
             "type": "PremiumStash", "hidden": true}
        ],
        "items": []
    })
    .to_string();
    let items_body = json!({
        "items": [
            {"id": "i1", "name": "Headhunter", "typeLine": "Leather Belt",
             "baseType": "Leather Belt", "frameType": 3}
        ]
    })
    .to_string();

    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in [tabs_body, items_body] {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(read_request(&mut stream));
            stream.write_all(response(&body).as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        requests
    });

    let client = OwnClient::with_base(&addr, "session-cookie-value", "me@example.com");
    let tabs = client.tabs("Tester#1234", "Standard").unwrap();
    assert_eq!(tabs.tabs.len(), 2);
    assert!(tabs.tabs[1].hidden);
    let items = client.tab("Tester#1234", "Standard", 0).unwrap();
    assert_eq!(items.len(), 1);

    let requests = handle.join().unwrap();
    assert!(requests[0].starts_with(
        "GET /character-window/get-stash-items?accountName=Tester%231234&league=Standard&tabs=1&tabIndex=0 "
    ));
    assert!(requests[1].contains("tabs=0&tabIndex=0"));
    assert!(requests[0].contains("cookie: POESESSID=session-cookie-value"));
}
