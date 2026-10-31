//! SQLite persistence: the stash/item index plus the listing event log.
//!
//! The stream only sends stashes that *changed*, and each change carries the
//! full current contents of that stash. That lets us diff a stash against what
//! we recorded and derive listing events: `listed`, `price_changed`,
//! `relisted`, `delisted`, `stash_unlisted`.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::model::Stash;
use crate::price::{self, Price};

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS stashes (
    id          TEXT PRIMARY KEY,
    account     TEXT,
    name        TEXT,
    stash_type  TEXT,
    league      TEXT,
    public      INTEGER NOT NULL DEFAULT 1,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    delisted_at INTEGER
);
CREATE INDEX IF NOT EXISTS stashes_league ON stashes(league);

CREATE TABLE IF NOT EXISTS items (
    id             TEXT PRIMARY KEY,
    stash_id       TEXT NOT NULL,
    league         TEXT,
    account        TEXT,
    name           TEXT,
    type_line      TEXT,
    base_type      TEXT,
    rarity         TEXT,
    frame_type     INTEGER,
    ilvl           INTEGER,
    stack_size     INTEGER,
    sockets        TEXT,
    links          INTEGER,
    corrupted      INTEGER NOT NULL DEFAULT 0,
    identified     INTEGER,
    icon           TEXT,
    note           TEXT,
    price_amount   REAL,
    price_currency TEXT,
    price_type     TEXT,
    mods_text      TEXT,
    first_seen     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL,
    delisted_at    INTEGER,
    raw            TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS items_stash  ON items(stash_id);
CREATE INDEX IF NOT EXISTS items_league ON items(league, base_type);
CREATE INDEX IF NOT EXISTS items_price  ON items(price_currency, price_amount);
CREATE INDEX IF NOT EXISTS items_seen   ON items(first_seen);
CREATE INDEX IF NOT EXISTS items_name   ON items(name, type_line);

CREATE TABLE IF NOT EXISTS listings (
    seq            INTEGER PRIMARY KEY,
    item_id        TEXT NOT NULL,
    stash_id       TEXT,
    league         TEXT,
    account        TEXT,
    event          TEXT NOT NULL,
    price_amount   REAL,
    price_currency TEXT,
    old_amount     REAL,
    old_currency   TEXT,
    at             INTEGER NOT NULL,
    change_id      TEXT
);
CREATE INDEX IF NOT EXISTS listings_item ON listings(item_id, at);
CREATE INDEX IF NOT EXISTS listings_time ON listings(at);
"#;

const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
    name, type_line, base_type, mods_text,
    content='items', content_rowid='rowid'
);
CREATE TRIGGER IF NOT EXISTS items_fts_ai AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, name, type_line, base_type, mods_text)
    VALUES (new.rowid, new.name, new.type_line, new.base_type, new.mods_text);
END;
CREATE TRIGGER IF NOT EXISTS items_fts_ad AFTER DELETE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, type_line, base_type, mods_text)
    VALUES ('delete', old.rowid, old.name, old.type_line, old.base_type, old.mods_text);
END;
CREATE TRIGGER IF NOT EXISTS items_fts_au AFTER UPDATE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, type_line, base_type, mods_text)
    VALUES ('delete', old.rowid, old.name, old.type_line, old.base_type, old.mods_text);
    INSERT INTO items_fts(rowid, name, type_line, base_type, mods_text)
    VALUES (new.rowid, new.name, new.type_line, new.base_type, new.mods_text);
END;
"#;

pub fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestStats {
    pub stashes: usize,
    pub items_new: usize,
    pub price_changes: usize,
    pub relisted: usize,
    pub delisted: usize,
    pub unlisted_stashes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemRow {
    pub id: String,
    pub stash_id: String,
    pub stash_name: Option<String>,
    pub account: Option<String>,
    pub league: Option<String>,
    pub name: Option<String>,
    pub type_line: Option<String>,
    pub base_type: Option<String>,
    pub rarity: Option<String>,
    pub ilvl: Option<i64>,
    pub stack_size: Option<i64>,
    pub sockets: Option<String>,
    pub links: Option<i64>,
    pub corrupted: bool,
    pub note: Option<String>,
    pub price_amount: Option<f64>,
    pub price_currency: Option<String>,
    pub price_type: Option<String>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub delisted_at: Option<i64>,
}

impl ItemRow {
    pub fn price(&self) -> Option<String> {
        let amount = self.price_amount?;
        let currency = self.price_currency.clone().unwrap_or_default();
        let kind = self.price_type.clone().unwrap_or_else(|| "b/o".into());
        Some(format!("{} {} {}", kind, trim_num(amount), currency))
    }

    pub fn display_name(&self) -> String {
        match (&self.name, &self.type_line) {
            (Some(n), Some(t)) if !n.is_empty() && n != t => format!("{n} ({t})"),
            (Some(n), _) if !n.is_empty() => n.clone(),
            (_, Some(t)) => t.clone(),
            _ => "<unnamed>".into(),
        }
    }
}

fn trim_num(v: f64) -> String {
    if (v.fract()).abs() < f64::EPSILON {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    pub event: String,
    pub at: i64,
    pub price_amount: Option<f64>,
    pub price_currency: Option<String>,
    pub old_amount: Option<f64>,
    pub old_currency: Option<String>,
    pub stash_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Newest,
    Oldest,
    PriceAsc,
    PriceDesc,
    IlvlDesc,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub league: Option<String>,
    pub name: Option<String>,
    pub base: Option<String>,
    pub rarity: Option<String>,
    pub account: Option<String>,
    pub mods: Option<String>,
    pub min_ilvl: Option<i64>,
    pub max_ilvl: Option<i64>,
    pub min_price: Option<f64>,
    pub max_price: Option<f64>,
    pub currency: Option<String>,
    pub min_links: Option<i64>,
    pub since: Option<i64>,
    pub include_delisted: bool,
    pub limit: usize,
    pub offset: usize,
    pub order: Order,
}

impl Default for SearchQuery {
    fn default() -> Self {
        SearchQuery {
            league: None,
            name: None,
            base: None,
            rarity: None,
            account: None,
            mods: None,
            min_ilvl: None,
            max_ilvl: None,
            min_price: None,
            max_price: None,
            currency: None,
            min_links: None,
            since: None,
            include_delisted: false,
            limit: 25,
            offset: 0,
            order: Order::Newest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeagueStat {
    pub league: String,
    pub active_items: i64,
    pub priced_items: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub next_change_id: Option<String>,
    pub pages: i64,
    pub last_poll: Option<i64>,
    pub first_index: Option<i64>,
    pub stashes: i64,
    pub active_stashes: i64,
    pub items: i64,
    pub active_items: i64,
    pub priced_items: i64,
    pub events: i64,
    pub leagues: Vec<LeagueStat>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating database directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::from_conn(conn)
    }

    pub fn open_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        // FTS5 ships with the bundled SQLite; if a system build lacks it we
        // still work, just with substring matching on mods_text.
        let fts = conn.execute_batch(FTS_SCHEMA).is_ok();
        let mut store = Store { conn };
        store.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        store.set_meta("fts5", if fts { "1" } else { "0" })?;
        Ok(store)
    }

    fn has_fts(&self) -> bool {
        self.meta("fts5").ok().flatten().as_deref() == Some("1")
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn next_change_id(&self) -> Result<Option<String>> {
        self.meta("next_change_id")
    }

    /// Drop the cursor so the next poll restarts from the stream head. Used
    /// when the API rejects a cursor (e.g. after a long outage).
    pub fn clear_cursor(&mut self) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = 'next_change_id'", [])?;
        Ok(())
    }

    /// Apply one page of the stream. The page's `next_change_id` cursor and the
    /// page contents move forward in the same transaction, so a crash can never
    /// skip a page.
    pub fn ingest_page(
        &mut self,
        page: &crate::model::StashPage,
        advance_cursor: bool,
    ) -> Result<IngestStats> {
        let now = unix_now();
        let change_id = page.next_change_id.clone();
        let tx = self.conn.transaction()?;
        let mut stats = IngestStats::default();

        for change in &page.stashes {
            let stash = change.clone().into_stash();
            ingest_stash(&tx, &stash, &change_id, now, &mut stats)?;
        }

        if advance_cursor {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES ('next_change_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [&change_id],
            )?;
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('last_poll', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now.to_string()],
        )?;
        let pages: i64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'pages'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            + 1;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('pages', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [pages.to_string()],
        )?;
        if tx
            .query_row(
                "SELECT value FROM meta WHERE key = 'first_index'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_none()
        {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES ('first_index', ?1)",
                [now.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(stats)
    }

    pub fn search(&self, q: &SearchQuery) -> Result<Vec<ItemRow>> {
        let mut sql = String::from(ITEM_SELECT);
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();

        if !q.include_delisted {
            clauses.push("i.delisted_at IS NULL".into());
        }
        if let Some(v) = &q.league {
            clauses.push("i.league = ?".into());
            args.push(v.clone().into());
        }
        if let Some(v) = &q.name {
            clauses.push("i.name LIKE ? COLLATE NOCASE".into());
            args.push(format!("%{v}%").into());
        }
        if let Some(v) = &q.base {
            clauses.push(
                "(i.base_type LIKE ? COLLATE NOCASE OR i.type_line LIKE ? COLLATE NOCASE)".into(),
            );
            args.push(format!("%{v}%").into());
            args.push(format!("%{v}%").into());
        }
        if let Some(v) = &q.rarity {
            clauses.push("i.rarity = ? COLLATE NOCASE".into());
            args.push(v.clone().into());
        }
        if let Some(v) = &q.account {
            clauses.push("i.account = ? COLLATE NOCASE".into());
            args.push(v.clone().into());
        }
        if let Some(v) = &q.mods {
            if self.has_fts() {
                clauses.push(
                    "i.rowid IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)".into(),
                );
                args.push(fts_query(v).into());
            } else {
                clauses.push("i.mods_text LIKE ? COLLATE NOCASE".into());
                args.push(format!("%{v}%").into());
            }
        }
        if let Some(v) = q.min_ilvl {
            clauses.push("i.ilvl >= ?".into());
            args.push(v.into());
        }
        if let Some(v) = q.max_ilvl {
            clauses.push("i.ilvl <= ?".into());
            args.push(v.into());
        }
        if let Some(v) = q.min_price {
            clauses.push("i.price_amount >= ?".into());
            args.push(v.into());
        }
        if let Some(v) = q.max_price {
            clauses.push("i.price_amount <= ?".into());
            args.push(v.into());
        }
        if let Some(v) = &q.currency {
            clauses.push("i.price_currency = ? COLLATE NOCASE".into());
            args.push(v.clone().into());
        }
        if let Some(v) = q.min_links {
            clauses.push("i.links >= ?".into());
            args.push(v.into());
        }
        if let Some(v) = q.since {
            clauses.push("i.first_seen >= ?".into());
            args.push(v.into());
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(match q.order {
            Order::Newest => " ORDER BY i.first_seen DESC, i.rowid DESC",
            Order::Oldest => " ORDER BY i.first_seen ASC, i.rowid ASC",
            Order::PriceAsc => " ORDER BY i.price_amount IS NULL, i.price_amount ASC",
            Order::PriceDesc => " ORDER BY i.price_amount IS NULL, i.price_amount DESC",
            Order::IlvlDesc => " ORDER BY i.ilvl IS NULL, i.ilvl DESC",
        });
        sql.push_str(" LIMIT ? OFFSET ?");
        args.push((q.limit as i64).into());
        args.push((q.offset as i64).into());

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), item_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mods text and the exact JSON for one item, kept out of `ITEM_SELECT`
    /// because they are large and only the detail view needs them.
    pub fn item_extra(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(mods_text, ''), raw FROM items WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn item(&self, id: &str) -> Result<Option<ItemRow>> {
        Ok(self
            .conn
            .query_row(&format!("{ITEM_SELECT} WHERE i.id = ?1"), [id], item_row)
            .optional()?)
    }

    pub fn history(&self, item_id: &str) -> Result<Vec<Listing>> {
        let mut stmt = self.conn.prepare(
            "SELECT event, at, price_amount, price_currency, old_amount, old_currency, stash_id
             FROM listings WHERE item_id = ?1 ORDER BY at ASC, seq ASC",
        )?;
        let rows = stmt
            .query_map([item_id], |r| {
                Ok(Listing {
                    event: r.get(0)?,
                    at: r.get(1)?,
                    price_amount: r.get(2)?,
                    price_currency: r.get(3)?,
                    old_amount: r.get(4)?,
                    old_currency: r.get(5)?,
                    stash_id: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn stats(&self) -> Result<Stats> {
        let count = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let mut stats = Stats {
            next_change_id: self.next_change_id()?,
            pages: self
                .meta("pages")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            last_poll: self.meta("last_poll")?.and_then(|v| v.parse().ok()),
            first_index: self.meta("first_index")?.and_then(|v| v.parse().ok()),
            stashes: count("SELECT COUNT(*) FROM stashes")?,
            active_stashes: count("SELECT COUNT(*) FROM stashes WHERE public = 1")?,
            items: count("SELECT COUNT(*) FROM items")?,
            active_items: count("SELECT COUNT(*) FROM items WHERE delisted_at IS NULL")?,
            priced_items: count(
                "SELECT COUNT(*) FROM items WHERE delisted_at IS NULL AND price_amount IS NOT NULL",
            )?,
            events: count("SELECT COUNT(*) FROM listings")?,
            leagues: Vec::new(),
        };

        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(league, '?') AS l,
                    SUM(CASE WHEN delisted_at IS NULL THEN 1 ELSE 0 END),
                    SUM(CASE WHEN delisted_at IS NULL AND price_amount IS NOT NULL THEN 1 ELSE 0 END)
             FROM items GROUP BY l ORDER BY 2 DESC LIMIT 20",
        )?;
        stats.leagues = stmt
            .query_map([], |r| {
                Ok(LeagueStat {
                    league: r.get(0)?,
                    active_items: r.get(1)?,
                    priced_items: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(stats)
    }
}

const ITEM_SELECT: &str =
    "SELECT i.id, i.stash_id, s.name, i.account, i.league, i.name, i.type_line,
        i.base_type, i.rarity, i.ilvl, i.stack_size, i.sockets, i.links, i.corrupted, i.note,
        i.price_amount, i.price_currency, i.price_type, i.first_seen, i.last_seen, i.delisted_at
    FROM items i LEFT JOIN stashes s ON s.id = i.stash_id";

fn item_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    Ok(ItemRow {
        id: r.get(0)?,
        stash_id: r.get(1)?,
        stash_name: r.get(2)?,
        account: r.get(3)?,
        league: r.get(4)?,
        name: r.get(5)?,
        type_line: r.get(6)?,
        base_type: r.get(7)?,
        rarity: r.get(8)?,
        ilvl: r.get(9)?,
        stack_size: r.get(10)?,
        sockets: r.get(11)?,
        links: r.get(12)?,
        corrupted: r.get::<_, i64>(13)? != 0,
        note: r.get(14)?,
        price_amount: r.get(15)?,
        price_currency: r.get(16)?,
        price_type: r.get(17)?,
        first_seen: r.get(18)?,
        last_seen: r.get(19)?,
        delisted_at: r.get(20)?,
    })
}

/// FTS5 query syntax is its own language; escape user input so that a mod
/// search like `+40% to Fire` cannot raise a syntax error.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone)]
struct ExistingItem {
    price_amount: Option<f64>,
    price_currency: Option<String>,
    delisted_at: Option<i64>,
}

fn ingest_stash(
    tx: &Transaction<'_>,
    stash: &Stash,
    change_id: &str,
    now: i64,
    stats: &mut IngestStats,
) -> Result<()> {
    stats.stashes += 1;

    tx.execute(
        "INSERT INTO stashes(id, account, name, stash_type, league, public, first_seen, last_seen, delisted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, NULL)
         ON CONFLICT(id) DO UPDATE SET
             account    = COALESCE(excluded.account, stashes.account),
             name       = COALESCE(excluded.name, stashes.name),
             stash_type = COALESCE(excluded.stash_type, stashes.stash_type),
             league     = COALESCE(excluded.league, stashes.league),
             public     = excluded.public,
             last_seen  = excluded.last_seen",
        params![
            stash.id,
            stash.account,
            stash.name,
            stash.stash_type,
            stash.league,
            stash.public as i64,
            now
        ],
    )?;

    let open_items = {
        let mut stmt = tx.prepare(
            "SELECT id, price_amount, price_currency, delisted_at
             FROM items WHERE stash_id = ?1 AND delisted_at IS NULL",
        )?;
        let map: HashMap<String, ExistingItem> = stmt
            .query_map([&stash.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ExistingItem {
                        price_amount: r.get(1)?,
                        price_currency: r.get(2)?,
                        delisted_at: r.get(3)?,
                    },
                ))
            })?
            .collect::<Result<HashMap<_, _>, _>>()?;
        map
    };

    if !stash.public {
        // The unlist event carries no items: close the whole stash out.
        tx.execute(
            "UPDATE stashes SET delisted_at = ?2 WHERE id = ?1",
            params![stash.id, now],
        )?;
        for item_id in open_items.keys() {
            close_item(tx, item_id, &stash.id, stash, change_id, now)?;
            stats.delisted += 1;
        }
        stats.unlisted_stashes += 1;
        return Ok(());
    }

    tx.execute(
        "UPDATE stashes SET delisted_at = NULL WHERE id = ?1",
        params![stash.id],
    )?;

    let incoming: HashSet<&str> = stash.items.iter().map(|i| i.id.as_str()).collect();

    for item in &stash.items {
        let parsed = item.note.as_deref().and_then(price::parse_note);
        let (amount, currency, kind) = match &parsed {
            Some(p) => (
                Some(p.amount),
                Some(p.currency.as_str()),
                Some(p.kind.as_str()),
            ),
            None => (None, None, None),
        };

        let existing = open_items.get(&item.id);
        // The item may exist in another stash (moved between tabs) or be
        // currently delisted; either way it is not a new listing.
        let previous = match existing {
            Some(e) => Some(e.clone()),
            None => select_item(tx, &item.id)?,
        };

        upsert_item(tx, item, stash, amount, currency, kind, now)?;
        stats.items_new += usize::from(previous.is_none());

        match &previous {
            None => insert_event(
                tx,
                item.id.as_str(),
                stash,
                "listed",
                &parsed,
                None,
                change_id,
                now,
            )?,
            Some(prev) => {
                if prev.delisted_at.is_some() {
                    stats.relisted += 1;
                    insert_event(
                        tx,
                        item.id.as_str(),
                        stash,
                        "relisted",
                        &parsed,
                        None,
                        change_id,
                        now,
                    )?;
                }
                if price_changed(prev, amount, currency) {
                    stats.price_changes += 1;
                    insert_event(
                        tx,
                        item.id.as_str(),
                        stash,
                        "price_changed",
                        &parsed,
                        Some(prev),
                        change_id,
                        now,
                    )?;
                }
            }
        }
    }

    // Anything we had open for this stash that the new snapshot does not
    // contain was removed or sold.
    for (item_id, prev) in &open_items {
        if !incoming.contains(item_id.as_str()) {
            close_item(tx, item_id, &stash.id, stash, change_id, now)?;
            let _ = prev;
            stats.delisted += 1;
        }
    }

    Ok(())
}

fn price_changed(prev: &ExistingItem, amount: Option<f64>, currency: Option<&str>) -> bool {
    let same_amount = match (prev.price_amount, amount) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    };
    let same_currency = match (prev.price_currency.as_deref(), currency) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    };
    !(same_amount && same_currency)
}

fn select_item(tx: &Transaction<'_>, item_id: &str) -> Result<Option<ExistingItem>> {
    Ok(tx
        .query_row(
            "SELECT price_amount, price_currency, delisted_at FROM items WHERE id = ?1",
            [item_id],
            |r| {
                Ok(ExistingItem {
                    price_amount: r.get(0)?,
                    price_currency: r.get(1)?,
                    delisted_at: r.get(2)?,
                })
            },
        )
        .optional()?)
}

#[allow(clippy::too_many_arguments)]
fn upsert_item(
    tx: &Transaction<'_>,
    item: &crate::model::Item,
    stash: &Stash,
    amount: Option<f64>,
    currency: Option<&str>,
    kind: Option<&str>,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO items(
            id, stash_id, league, account, name, type_line, base_type, rarity, frame_type, ilvl,
            stack_size, sockets, links, corrupted, identified, icon, note, price_amount,
            price_currency, price_type, mods_text, first_seen, last_seen, delisted_at, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?22, NULL, ?23)
         ON CONFLICT(id) DO UPDATE SET
            stash_id       = excluded.stash_id,
            league         = COALESCE(excluded.league, items.league),
            account        = COALESCE(excluded.account, items.account),
            name           = excluded.name,
            type_line      = excluded.type_line,
            base_type      = excluded.base_type,
            rarity         = excluded.rarity,
            frame_type     = excluded.frame_type,
            ilvl           = excluded.ilvl,
            stack_size     = excluded.stack_size,
            sockets        = excluded.sockets,
            links          = excluded.links,
            corrupted      = excluded.corrupted,
            identified     = excluded.identified,
            icon           = excluded.icon,
            note           = excluded.note,
            price_amount   = excluded.price_amount,
            price_currency = excluded.price_currency,
            price_type     = excluded.price_type,
            mods_text      = excluded.mods_text,
            last_seen      = excluded.last_seen,
            delisted_at    = NULL,
            raw            = excluded.raw",
        params![
            item.id,
            stash.id,
            stash.league,
            stash.account,
            item.name,
            item.type_line,
            item.base_type,
            item.rarity,
            item.frame_type,
            item.ilvl,
            item.stack_size,
            item.sockets,
            item.links,
            item.corrupted as i64,
            item.identified.map(|b| b as i64),
            item.icon,
            item.note,
            amount,
            currency,
            kind,
            item.mods_text,
            now,
            item.raw,
        ],
    )?;
    Ok(())
}

fn close_item(
    tx: &Transaction<'_>,
    item_id: &str,
    stash_id: &str,
    stash: &Stash,
    change_id: &str,
    now: i64,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE items SET delisted_at = ?2 WHERE id = ?1 AND delisted_at IS NULL",
        params![item_id, now],
    )?;
    if changed > 0 {
        let price = {
            let mut stmt =
                tx.prepare("SELECT price_amount, price_currency FROM items WHERE id = ?1")?;
            stmt.query_row([item_id], |r| {
                Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?
        };
        let old = price.and_then(|(a, c)| {
            Some(Price {
                kind: "b/o".into(),
                amount: a?,
                currency: c?,
            })
        });
        insert_event(tx, item_id, stash, "delisted", &old, None, change_id, now)?;
        let _ = stash_id;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_event(
    tx: &Transaction<'_>,
    item_id: &str,
    stash: &Stash,
    event: &str,
    price: &Option<Price>,
    previous: Option<&ExistingItem>,
    change_id: &str,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO listings(item_id, stash_id, league, account, event, price_amount,
                               price_currency, old_amount, old_currency, at, change_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            item_id,
            stash.id,
            stash.league,
            stash.account,
            event,
            price.as_ref().map(|p| p.amount),
            price.as_ref().map(|p| p.currency.as_str()),
            previous.and_then(|p| p.price_amount),
            previous.and_then(|p| p.price_currency.as_deref()),
            now,
            change_id,
        ],
    )?;
    Ok(())
}
