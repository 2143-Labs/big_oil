//! End-to-end tests over the ingest/diff/query pipeline, using one real
//! public-stash page (item JSON captured from the live API) and mutations of it
//! to reproduce the events the stream emits.

use big_oil::model::StashPage;
use big_oil::store::{Order, SearchQuery, Store};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/page_1.json")).unwrap()
}

fn parse(value: &Value) -> StashPage {
    StashPage::parse(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn store() -> Store {
    Store::open_memory().unwrap()
}

fn all(store: &Store) -> Vec<big_oil::store::ItemRow> {
    store
        .search(&SearchQuery {
            include_delisted: true,
            limit: 100,
            ..Default::default()
        })
        .unwrap()
}

fn find<'a>(rows: &'a [big_oil::store::ItemRow], needle: &str) -> &'a big_oil::store::ItemRow {
    rows.iter()
        .find(|r| r.display_name().contains(needle))
        .unwrap_or_else(|| panic!("no item matching {needle:?}"))
}

#[test]
fn ingests_page_and_records_first_listings() {
    let mut store = store();
    let stats = store.ingest_page(&parse(&fixture()), true).unwrap();
    assert_eq!(stats.stashes, 3);
    assert_eq!(stats.items_new, 6);
    assert_eq!(stats.price_changes, 0);
    assert_eq!(stats.delisted, 0);

    let rows = all(&store);
    assert_eq!(rows.len(), 6);

    let unique = find(&rows, "Headhunter");
    assert_eq!(unique.price().as_deref(), Some("b/o 120 chaos"));
    assert_eq!(unique.rarity.as_deref(), Some("Unique"));
    assert_eq!(unique.ilvl, Some(85));
    assert_eq!(unique.league.as_deref(), Some("Standard"));
    assert_eq!(unique.stash_name.as_deref(), Some("Dump"));
    assert!(unique.delisted_at.is_none());

    let history = store.history(&unique.id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event, "listed");
    assert_eq!(history[0].price_amount, Some(120.0));
    assert_eq!(history[0].price_currency.as_deref(), Some("chaos"));
}

#[test]
fn indexes_mods_from_structured_and_plain_fields() {
    let mut store = store();
    store.ingest_page(&parse(&fixture()), true).unwrap();

    // Object-form mods (`{"description": ...}`) come from the live schema.
    // Both Tricorne rares in the fixture carry this mod.
    let rows = store
        .search(&SearchQuery {
            mods: Some("Socketed Minion Gems".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|r| r.display_name().contains("Dragon Veil"))
    );
    assert!(rows.iter().any(|r| r.display_name().contains("Doom Crest")));

    // A mod that no item has returns nothing, rather than a syntax error.
    assert!(
        store
            .search(&SearchQuery {
                mods: Some("+40% to Nothing At All".into()),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn mods_search_ignores_item_names() {
    let mut store = store();
    store.ingest_page(&parse(&fixture()), true).unwrap();
    let rows = store
        .search(&SearchQuery {
            mods: Some("Headhunter".into()),
            ..Default::default()
        })
        .unwrap();
    // The Doctor's mod line names Headhunter (`<uniqueitem>{Headhunter}`); the
    // belt called Headhunter has no such mod and must not match.
    let names: Vec<String> = rows.iter().map(|r| r.display_name()).collect();
    assert_eq!(names, vec!["The Doctor"], "--mods searches mod text only");
}

#[test]
fn detects_price_changes_without_creating_a_new_item() {
    let mut store = store();
    let mut page = fixture();
    store.ingest_page(&parse(&page), true).unwrap();

    let unique_id = page["stashes"][0]["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    page["stashes"][0]["items"][0]["note"] = Value::String("~b/o 90 chaos".into());
    let stats = store.ingest_page(&parse(&page), true).unwrap();

    assert_eq!(stats.items_new, 0);
    assert_eq!(stats.price_changes, 1);
    assert_eq!(all(&store).len(), 6);

    let item = store.item(&unique_id).unwrap().unwrap();
    assert_eq!(item.price().as_deref(), Some("b/o 90 chaos"));

    let history = store.history(&unique_id).unwrap();
    assert_eq!(
        history.iter().map(|h| h.event.as_str()).collect::<Vec<_>>(),
        vec!["listed", "price_changed"]
    );
    assert_eq!(history[1].old_amount, Some(120.0));
    assert_eq!(history[1].price_amount, Some(90.0));
}

#[test]
fn item_missing_from_a_stash_snapshot_is_delisted() {
    let mut store = store();
    let mut page = fixture();
    store.ingest_page(&parse(&page), true).unwrap();

    let removed = page["stashes"][0]["items"][1].as_object().unwrap().clone();
    let removed_id = removed["id"].as_str().unwrap().to_owned();
    page["stashes"][0]["items"] = Value::Array(vec![page["stashes"][0]["items"][0].clone()]);
    let stats = store.ingest_page(&parse(&page), true).unwrap();
    assert_eq!(stats.delisted, 1);
    assert_eq!(stats.unlisted_stashes, 0);

    let item = store.item(&removed_id).unwrap().unwrap();
    assert!(item.delisted_at.is_some());
    assert_eq!(all(&store).len(), 6, "delisted items stay in the index");

    let live = store.search(&SearchQuery::default()).unwrap();
    assert_eq!(live.len(), 5, "default search hides delisted items");

    let history = store.history(&removed_id).unwrap();
    assert_eq!(history.last().unwrap().event, "delisted");
}

#[test]
fn unlisting_a_stash_closes_all_its_items() {
    let mut store = store();
    let mut page = fixture();
    store.ingest_page(&parse(&page), true).unwrap();

    let stash_id = page["stashes"][0]["id"].as_str().unwrap().to_owned();
    let item_ids: Vec<String> = page["stashes"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect();

    page["stashes"][0] = serde_json::json!({"id": stash_id, "public": false});
    let stats = store.ingest_page(&parse(&page), true).unwrap();
    assert_eq!(stats.unlisted_stashes, 1);
    assert_eq!(stats.delisted, 2);

    for id in &item_ids {
        let item = store.item(id).unwrap().unwrap();
        assert!(item.delisted_at.is_some(), "{id} should be delisted");
        assert_eq!(store.history(id).unwrap().last().unwrap().event, "delisted");
    }
    assert!(store.search(&SearchQuery::default()).unwrap().len() == 4);
}

#[test]
fn relisting_a_stash_marks_items_relisted() {
    let mut store = store();
    let mut page = fixture();
    store.ingest_page(&parse(&page), true).unwrap();

    let original = page["stashes"][0].clone();
    let stash_id = original["id"].as_str().unwrap().to_owned();
    page["stashes"][0] = serde_json::json!({"id": stash_id, "public": false});
    store.ingest_page(&parse(&page), true).unwrap();

    let mut relisted = original.clone();
    relisted["items"][0]["note"] = Value::String("~b/o 100 chaos".into());
    page["stashes"][0] = relisted;
    let stats = store.ingest_page(&parse(&page), true).unwrap();
    assert_eq!(stats.relisted, 2);
    assert_eq!(stats.price_changes, 1);

    let id = original["items"][0]["id"].as_str().unwrap();
    let item = store.item(id).unwrap().unwrap();
    assert!(item.delisted_at.is_none());
    let events: Vec<_> = store
        .history(id)
        .unwrap()
        .into_iter()
        .map(|h| h.event)
        .collect();
    assert_eq!(
        events,
        vec!["listed", "delisted", "relisted", "price_changed"]
    );
}

#[test]
fn cursor_advances_only_when_asked() {
    let mut store = store();
    let mut page = fixture();
    page["next_change_id"] = Value::String("first".into());
    store.ingest_page(&parse(&page), true).unwrap();
    assert_eq!(store.next_change_id().unwrap().as_deref(), Some("first"));

    page["next_change_id"] = Value::String("replayed".into());
    store.ingest_page(&parse(&page), false).unwrap();
    assert_eq!(
        store.next_change_id().unwrap().as_deref(),
        Some("first"),
        "replaying a fixture must not move the live cursor"
    );
}

#[test]
fn search_filters_apply() {
    let mut store = store();
    store.ingest_page(&parse(&fixture()), true).unwrap();

    let by_name = store
        .search(&SearchQuery {
            name: Some("head".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_name.len(), 1);

    let by_base = store
        .search(&SearchQuery {
            base: Some("tricorne".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_base.len(), 2, "both Tricorne rares match");

    let cheap_chaos = store
        .search(&SearchQuery {
            max_price: Some(20.0),
            currency: Some("chaos".into()),
            order: Order::PriceAsc,
            ..Default::default()
        })
        .unwrap();
    let prices: Vec<f64> = cheap_chaos.iter().filter_map(|r| r.price_amount).collect();
    assert_eq!(prices, vec![3.0, 15.0], "price-ascending, divine excluded");

    let hardcore = store
        .search(&SearchQuery {
            league: Some("Hardcore".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hardcore.len(), 1);
    assert_eq!(hardcore[0].name.as_deref(), Some("Doom Crest"));

    let high_ilvl = store
        .search(&SearchQuery {
            min_ilvl: Some(80),
            order: Order::IlvlDesc,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(high_ilvl.len(), 2);

    let paged = store
        .search(&SearchQuery {
            limit: 2,
            offset: 2,
            order: Order::Oldest,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(paged.len(), 2);
}

#[test]
fn stats_summarise_the_index() {
    let mut store = store();
    store.ingest_page(&parse(&fixture()), true).unwrap();

    let stats = store.stats().unwrap();
    assert_eq!(stats.pages, 1);
    assert_eq!(stats.stashes, 3);
    assert_eq!(stats.items, 6);
    assert_eq!(stats.active_items, 6);
    assert_eq!(stats.priced_items, 6);
    assert_eq!(stats.events, 6);
    assert_eq!(stats.leagues.len(), 2);
    assert_eq!(
        stats.next_change_id.as_deref(),
        Some("eJwVzDEKwzAQBMC_KF0KcdLJllyIYxvs2A-BK-1aB8aYkL9nd5jmmLXnVWqdXqcMnJ1O5Tdf3mQyP5jS")
    );
}

#[test]
fn unknown_fields_do_not_break_ingest() {
    let mut store = store();
    let mut page = fixture();
    page["future_field"] = serde_json::json!({"unexpected": true});
    page["stashes"][0]["items"][0]["brandNewProperty"] = serde_json::json!([1, 2, 3]);
    let stats = store.ingest_page(&parse(&page), true).unwrap();
    assert_eq!(stats.items_new, 6);
}
