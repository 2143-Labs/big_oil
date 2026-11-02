//! SQLite persistence: the stash/item index plus the listing event log.
//!
//! The public stash stream and the account's own tabs live in separate
//! database files with the same schema. `stream` rows are the public firehose
//! (`listed` / `price_changed` / `relisted` / `delisted`); `own` rows are your
//! tabs (`dropped` / `price_changed` / `restored` / `removed`). Either way a
//! tab arrives with its full contents, so successive snapshots are diffed.
//!
//! A price comes from the item's own note or, when it has none, from the tab
//! name (`~price 1 divine`): the game applies a priced tab name to every item
//! in it that has no note of its own.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::model::Stash;
use crate::ninja::{self, ItemView, PriceLine};
use crate::price::{self, Price};

pub const SCHEMA_VERSION: i64 = 3;

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
    source      TEXT NOT NULL DEFAULT 'stream',
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    delisted_at INTEGER,
    tab_index   INTEGER,
    -- When the tab name last started parsing as a new price.
    price_since INTEGER
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
    gem_level      INTEGER,
    gem_quality    INTEGER,
    map_tier       INTEGER,
    x              INTEGER,
    y              INTEGER,
    w              INTEGER,
    h              INTEGER,
    source         TEXT NOT NULL DEFAULT 'stream',
    value_chaos    REAL,
    value_divine   REAL,
    valued_at      INTEGER,
    value_listings INTEGER,
    first_seen     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL,
    delisted_at    INTEGER,
    raw            TEXT NOT NULL,
    -- 1 when the price came from the tab name rather than the item's note.
    price_from_tab INTEGER NOT NULL DEFAULT 0,
    -- When the item first appeared or its effective price last changed.
    price_since    INTEGER,
    influences     TEXT,
    fractured      INTEGER NOT NULL DEFAULT 0,
    synthesised    INTEGER NOT NULL DEFAULT 0,
    prefixes       INTEGER,
    suffixes       INTEGER,
    frame_type_id  TEXT
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

-- Latest poe.ninja economy overview lines, one row per priced variant.
CREATE TABLE IF NOT EXISTS price_ref (
    seq           INTEGER PRIMARY KEY,
    league        TEXT NOT NULL,
    category      TEXT NOT NULL,
    name          TEXT,
    base_type     TEXT,
    variant       TEXT,
    links         INTEGER,
    corrupted     INTEGER,
    gem_level     INTEGER,
    gem_quality   INTEGER,
    map_tier      INTEGER,
    chaos         REAL,
    divine        REAL,
    listing_count INTEGER,
    details_id     TEXT,
    fetched_at    INTEGER NOT NULL,
    item_class     INTEGER,
    level_required INTEGER,
    trade_id       TEXT
);
CREATE INDEX IF NOT EXISTS price_ref_lookup ON price_ref(league, name);
CREATE INDEX IF NOT EXISTS price_ref_cat    ON price_ref(league, category);

CREATE TABLE IF NOT EXISTS price_snapshots (
    league     TEXT NOT NULL,
    category   TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    etag       TEXT,
    line_count INTEGER NOT NULL,
    PRIMARY KEY (league, category, fetched_at)
);

-- History of the estimates attached to individual items.
CREATE TABLE IF NOT EXISTS valuations (
    seq           INTEGER PRIMARY KEY,
    item_id       TEXT NOT NULL,
    at            INTEGER NOT NULL,
    chaos         REAL,
    divine        REAL,
    matched       TEXT,
    listing_count INTEGER
);
CREATE INDEX IF NOT EXISTS valuations_item ON valuations(item_id, at);
"#;

const FTS_SCHEMA: &str = r#"
DROP TRIGGER IF EXISTS items_fts_au;
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
CREATE TRIGGER IF NOT EXISTS items_fts_au AFTER UPDATE OF name, type_line, base_type, mods_text ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name, type_line, base_type, mods_text)
    VALUES ('delete', old.rowid, old.name, old.type_line, old.base_type, old.mods_text);
    INSERT INTO items_fts(rowid, name, type_line, base_type, mods_text)
    VALUES (new.rowid, new.name, new.type_line, new.base_type, new.mods_text);
END;
"#;

pub fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Where a snapshot came from, which decides the event vocabulary written to
/// `listings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The public stash stream: items are listed for sale.
    Market,
    /// The account's own tabs: items appear and disappear as you move them.
    Own,
}

impl Origin {
    fn source(self) -> &'static str {
        match self {
            Origin::Market => "stream",
            Origin::Own => "own",
        }
    }

    fn listed(self) -> &'static str {
        match self {
            Origin::Market => "listed",
            Origin::Own => "dropped",
        }
    }

    fn delisted(self) -> &'static str {
        match self {
            Origin::Market => "delisted",
            Origin::Own => "removed",
        }
    }

    fn relisted(self) -> &'static str {
        match self {
            Origin::Market => "relisted",
            Origin::Own => "restored",
        }
    }
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

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
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
    pub source: String,
    pub value_chaos: Option<f64>,
    pub value_divine: Option<f64>,
    pub valued_at: Option<i64>,
    /// Supply for the matched market line, a rough liquidity signal.
    pub value_listings: Option<i64>,
    pub identified: Option<bool>,
    pub frame_type: Option<i64>,
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub w: Option<i64>,
    pub h: Option<i64>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub delisted_at: Option<i64>,
    /// When the item first appeared or its effective price last changed.
    /// Movement between tabs at the same price does not reset it.
    pub price_since: Option<i64>,
    /// The price came from the tab name, not the item's own note.
    pub price_from_tab: bool,
    pub stash_type: Option<String>,
    /// Position in the account's tab list (own tabs).
    pub tab_index: Option<i64>,
    /// When the tab name last changed to a new price.
    pub tab_price_since: Option<i64>,
    /// When the tab was last scanned.
    pub stash_last_seen: Option<i64>,
}

impl ItemRow {
    pub fn price(&self) -> Option<String> {
        let amount = self.price_amount?;
        let currency = self.price_currency.clone().unwrap_or_default();
        let kind = self.price_type.clone().unwrap_or_else(|| "b/o".into());
        Some(format!(
            "{} {} {}",
            kind,
            crate::render::num(amount),
            currency
        ))
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Valuation {
    pub at: i64,
    pub chaos: Option<f64>,
    pub divine: Option<f64>,
    pub matched: Option<String>,
    pub listing_count: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OwnTabRow {
    pub stash_id: String,
    pub name: String,
    pub league: String,
    pub index: Option<i64>,
    pub stash_type: Option<String>,
    /// When the tab name last changed to a new price.
    pub price_since: Option<i64>,
    pub items: i64,
    pub live: i64,
    pub value_chaos: f64,
    pub value_divine: f64,
    pub last_seen: Option<i64>,
}

/// One of the account's own tabs, as listed by the stash endpoint.
#[derive(Debug, Clone, Copy)]
pub struct OwnTab<'a> {
    pub stash_id: &'a str,
    pub index: i64,
    pub name: &'a str,
    pub stash_type: &'a str,
    pub league: &'a str,
    pub account: Option<&'a str>,
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
        migrate(&conn)?;
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

    /// Point the stream cursor at `id` (a `next_change_id`).
    pub fn set_cursor(&mut self, id: &str) -> Result<()> {
        self.set_meta("next_change_id", id)
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
            ingest_stash(&tx, &stash, Origin::Market, &change_id, now, &mut stats)?;
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
                if let Some(fts) = fts_query(v) {
                    clauses.push(
                        "i.rowid IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)".into(),
                    );
                    args.push(fts.into());
                }
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

    /// Every live own-tab item in `league` — what the reports work from.
    pub fn own_rows(&self, league: &str) -> Result<Vec<ItemRow>> {
        self.search(&SearchQuery {
            league: Some(league.to_string()),
            limit: 100_000,
            ..Default::default()
        })
    }

    /// Currency rates from the stored poe.ninja snapshot.
    pub fn rates(&self, league: &str) -> Result<crate::rates::Rates> {
        let lines = self.price_lines(league)?;
        if lines.is_empty() {
            anyhow::bail!(
                "no price snapshot for {league}: run `big_oil ninja fetch --league {league}` first"
            );
        }
        Ok(crate::rates::Rates::from_lines(&lines))
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

    /// Ingest one of the account's own tabs. Same diff as the public stream,
    /// different vocabulary: items are `dropped`, disappear as `removed`.
    pub fn ingest_own_tab(
        &mut self,
        tab: &OwnTab<'_>,
        items: &[crate::model::Item],
    ) -> Result<IngestStats> {
        let now = unix_now();
        let stash = Stash {
            id: tab.stash_id.to_owned(),
            public: true,
            account: tab.account.map(str::to_owned),
            name: Some(tab.name.to_owned()),
            stash_type: Some(tab.stash_type.to_owned()),
            league: Some(tab.league.to_owned()),
            index: Some(tab.index),
            items: items.to_vec(),
        };
        let tx = self.conn.transaction()?;
        let mut stats = IngestStats::default();
        ingest_stash(&tx, &stash, Origin::Own, "", now, &mut stats)?;
        tx.commit()?;
        Ok(stats)
    }

    /// Replace the stored economy lines for `(league, category)`.
    pub fn upsert_price_lines(
        &mut self,
        league: &str,
        category: &str,
        etag: Option<&str>,
        lines: &[PriceLine],
    ) -> Result<usize> {
        let fetched_at = lines.first().map(|l| l.fetched_at).unwrap_or_else(unix_now);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM price_ref WHERE league = ?1 AND category = ?2",
            params![league, category],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO price_ref(league, category, name, base_type, variant, links,
                    corrupted, gem_level, gem_quality, map_tier, chaos, divine, listing_count,
                    details_id, fetched_at, item_class, level_required, trade_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18)",
            )?;
            for line in lines {
                stmt.execute(params![
                    line.league,
                    line.category,
                    line.name,
                    line.base_type,
                    line.variant,
                    line.links,
                    line.corrupted.map(|b| b as i64),
                    line.gem_level,
                    line.gem_quality,
                    line.map_tier,
                    line.chaos,
                    line.divine,
                    line.listing_count,
                    line.details_id,
                    line.fetched_at,
                    line.item_class,
                    line.level_required,
                    line.trade_id,
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO price_snapshots(league, category, fetched_at, etag, line_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![league, category, fetched_at, etag, lines.len() as i64],
        )?;
        tx.commit()?;
        Ok(lines.len())
    }

    pub fn price_lines(&self, league: &str) -> Result<Vec<PriceLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT league, category, name, base_type, variant, links, corrupted, gem_level,
                    gem_quality, map_tier, chaos, divine, listing_count, details_id, fetched_at,
                    item_class, level_required, trade_id
             FROM price_ref WHERE league = ?1",
        )?;
        let rows = stmt
            .query_map([league], |r| {
                Ok(PriceLine {
                    league: r.get(0)?,
                    category: r.get(1)?,
                    name: r.get(2)?,
                    base_type: r.get(3)?,
                    variant: r.get(4)?,
                    links: r.get(5)?,
                    corrupted: r.get::<_, Option<i64>>(6)?.map(|v| v != 0),
                    gem_level: r.get(7)?,
                    gem_quality: r.get(8)?,
                    map_tier: r.get(9)?,
                    chaos: r.get(10)?,
                    divine: r.get(11)?,
                    listing_count: r.get(12)?,
                    details_id: r.get(13)?,
                    fetched_at: r.get(14)?,
                    item_class: r.get(15)?,
                    level_required: r.get(16)?,
                    trade_id: r.get(17)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// ETag of the most recent stored snapshot for `(league, category)`.
    pub fn price_etag(&self, league: &str, category: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT etag FROM price_snapshots WHERE league = ?1 AND category = ?2
                 ORDER BY fetched_at DESC LIMIT 1",
                params![league, category],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn priced_at(&self, league: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT MAX(fetched_at) FROM price_ref WHERE league = ?1",
                [league],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Attach a market value to every live own-tab item that has a match, and
    /// clear the value of items that no longer match anything.
    pub fn value_items(&mut self, league: &str) -> Result<usize> {
        let lines = self.price_lines(league)?;
        if lines.is_empty() {
            return Ok(0);
        }
        let now = unix_now();
        let targets = {
            let mut stmt = self.conn.prepare(
                "SELECT id, name, type_line, base_type, frame_type, corrupted, links,
                        gem_level, gem_quality, map_tier
                 FROM items WHERE source = 'own' AND league = ?1 AND delisted_at IS NULL",
            )?;
            let rows: Vec<(String, ItemView)> = stmt
                .query_map([league], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        ItemView {
                            name: r.get(1)?,
                            type_line: r.get(2)?,
                            base_type: r.get(3)?,
                            frame_type: r.get(4)?,
                            corrupted: r.get::<_, i64>(5)? != 0,
                            links: r.get(6)?,
                            gem_level: r.get(7)?,
                            gem_quality: r.get(8)?,
                            map_tier: r.get(9)?,
                        },
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let tx = self.conn.transaction()?;
        let mut valued = 0usize;
        for (id, view) in targets {
            let Some(line) = ninja::lookup(&lines, &view) else {
                tx.execute(
                    "UPDATE items SET value_chaos = NULL, value_divine = NULL, value_listings = NULL,
                                      valued_at = ?2
                     WHERE id = ?1 AND value_chaos IS NOT NULL",
                    params![id, now],
                )?;
                continue;
            };
            tx.execute(
                "UPDATE items SET value_chaos = ?2, value_divine = ?3, valued_at = ?4,
                                  value_listings = ?5 WHERE id = ?1",
                params![id, line.chaos, line.divine, now, line.listing_count],
            )?;
            let matched = format!(
                "{}:{}",
                line.category,
                line.name.clone().unwrap_or_else(|| "?".into())
            );
            // History records changes, not every pass.
            let latest: Option<(Option<f64>, Option<String>)> = tx
                .query_row(
                    "SELECT chaos, matched FROM valuations WHERE item_id = ?1
                     ORDER BY at DESC, seq DESC LIMIT 1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let unchanged = latest.is_some_and(|(chaos, prev)| {
                chaos.is_some_and(|c| (c - line.chaos).abs() <= 1e-9)
                    && prev.as_deref() == Some(matched.as_str())
            });
            if !unchanged {
                tx.execute(
                    "INSERT INTO valuations(item_id, at, chaos, divine, matched, listing_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        id,
                        now,
                        line.chaos,
                        line.divine,
                        matched,
                        line.listing_count
                    ],
                )?;
            }
            valued += 1;
        }
        tx.commit()?;
        Ok(valued)
    }

    pub fn valuations(&self, item_id: &str) -> Result<Vec<Valuation>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, chaos, divine, matched, listing_count FROM valuations
             WHERE item_id = ?1 ORDER BY at ASC, seq ASC",
        )?;
        let rows = stmt
            .query_map([item_id], |r| {
                Ok(Valuation {
                    at: r.get(0)?,
                    chaos: r.get(1)?,
                    divine: r.get(2)?,
                    matched: r.get(3)?,
                    listing_count: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Open own tabs in tab order, with the live market value of their stock.
    pub fn own_tabs(&self) -> Result<Vec<OwnTabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, COALESCE(s.name, '?'), COALESCE(s.league, '?'),
                    s.tab_index, s.stash_type, s.price_since,
                    COUNT(i.id),
                    SUM(CASE WHEN i.delisted_at IS NULL THEN 1 ELSE 0 END),
                    SUM(CASE WHEN i.delisted_at IS NULL
                        THEN COALESCE(i.value_chaos, 0) * COALESCE(i.stack_size, 1) ELSE 0 END),
                    SUM(CASE WHEN i.delisted_at IS NULL
                        THEN COALESCE(i.value_divine, 0) * COALESCE(i.stack_size, 1) ELSE 0 END),
                    MAX(s.last_seen)
             FROM stashes s LEFT JOIN items i ON i.stash_id = s.id
             WHERE s.source = 'own' AND s.delisted_at IS NULL
             GROUP BY s.id ORDER BY s.tab_index, s.name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(OwnTabRow {
                    stash_id: r.get(0)?,
                    name: r.get(1)?,
                    league: r.get(2)?,
                    index: r.get(3)?,
                    stash_type: r.get(4)?,
                    price_since: r.get(5)?,
                    items: r.get(6)?,
                    live: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    value_chaos: r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                    value_divine: r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                    last_seen: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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

const ITEM_SELECT: &str = "SELECT
        i.id AS id, i.stash_id AS stash_id, s.name AS stash_name, i.account AS account,
        i.league AS league, i.name AS name, i.type_line AS type_line, i.base_type AS base_type,
        i.rarity AS rarity, i.ilvl AS ilvl, i.stack_size AS stack_size, i.sockets AS sockets,
        i.links AS links, i.corrupted AS corrupted, i.note AS note,
        i.price_amount AS price_amount, i.price_currency AS price_currency,
        i.price_type AS price_type, i.source AS source, i.value_chaos AS value_chaos,
        i.value_divine AS value_divine, i.valued_at AS valued_at,
        i.value_listings AS value_listings, i.identified AS identified,
        i.frame_type AS frame_type, i.x AS x, i.y AS y, i.w AS w, i.h AS h,
        i.first_seen AS first_seen, i.last_seen AS last_seen, i.delisted_at AS delisted_at,
        i.price_since AS price_since, i.price_from_tab AS price_from_tab,
        s.stash_type AS stash_type, s.tab_index AS tab_index,
        s.price_since AS tab_price_since, s.last_seen AS stash_last_seen
    FROM items i LEFT JOIN stashes s ON s.id = i.stash_id";

fn item_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    Ok(ItemRow {
        id: r.get("id")?,
        stash_id: r.get("stash_id")?,
        stash_name: r.get("stash_name")?,
        account: r.get("account")?,
        league: r.get("league")?,
        name: r.get("name")?,
        type_line: r.get("type_line")?,
        base_type: r.get("base_type")?,
        rarity: r.get("rarity")?,
        ilvl: r.get("ilvl")?,
        stack_size: r.get("stack_size")?,
        sockets: r.get("sockets")?,
        links: r.get("links")?,
        corrupted: r.get::<_, i64>("corrupted")? != 0,
        note: r.get("note")?,
        price_amount: r.get("price_amount")?,
        price_currency: r.get("price_currency")?,
        price_type: r.get("price_type")?,
        source: r.get("source")?,
        value_chaos: r.get("value_chaos")?,
        value_divine: r.get("value_divine")?,
        valued_at: r.get("valued_at")?,
        value_listings: r.get("value_listings")?,
        identified: r.get::<_, Option<i64>>("identified")?.map(|v| v != 0),
        frame_type: r.get("frame_type")?,
        x: r.get("x")?,
        y: r.get("y")?,
        w: r.get("w")?,
        h: r.get("h")?,
        first_seen: r.get("first_seen")?,
        last_seen: r.get("last_seen")?,
        delisted_at: r.get("delisted_at")?,
        price_since: r.get("price_since")?,
        price_from_tab: r.get::<_, i64>("price_from_tab")? != 0,
        stash_type: r.get("stash_type")?,
        tab_index: r.get("tab_index")?,
        tab_price_since: r.get("tab_price_since")?,
        stash_last_seen: r.get("stash_last_seen")?,
    })
}

/// FTS5 query syntax is its own language; escape user input so that a mod
/// search like `+40% to Fire` cannot raise a syntax error. The column filter
/// keeps `--mods` to mod text (names and bases have their own flags).
fn fts_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split_whitespace()
        .map(|word| word.replace('"', ""))
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\""))
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(format!("mods_text : ({})", words.join(" ")))
}

#[derive(Debug, Clone)]
struct ExistingItem {
    price: Option<Price>,
    delisted_at: Option<i64>,
    raw: String,
}

const EXISTING_COLUMNS: &str = "price_amount, price_currency, price_type, delisted_at, raw";

/// Read `EXISTING_COLUMNS` starting at column `at`.
fn existing_item(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<ExistingItem> {
    let amount: Option<f64> = r.get(at)?;
    let currency: Option<String> = r.get(at + 1)?;
    let kind: Option<String> = r.get(at + 2)?;
    Ok(ExistingItem {
        price: amount.zip(currency).map(|(amount, currency)| Price {
            kind: kind.unwrap_or_else(|| "b/o".into()),
            amount,
            currency,
        }),
        delisted_at: r.get(at + 3)?,
        raw: r.get(at + 4)?,
    })
}

fn ingest_stash(
    tx: &Transaction<'_>,
    stash: &Stash,
    origin: Origin,
    change_id: &str,
    now: i64,
    stats: &mut IngestStats,
) -> Result<()> {
    stats.stashes += 1;

    // A priced tab name prices every item in the tab that has no note.
    let stored: Option<(Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT name, price_since FROM stashes WHERE id = ?1",
            [&stash.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let tab_price = stash.name.as_deref().and_then(price::parse_note);
    let price_since = match (&stash.name, stored) {
        // An unlist event carries no name: the tab's price did not change.
        (None, Some((_, since))) => since,
        (None, None) => None,
        (Some(_), None) => tab_price.as_ref().map(|_| now),
        (Some(_), Some((prev_name, since))) => {
            let prev = prev_name.as_deref().and_then(price::parse_note);
            if price::same_price(prev.as_ref(), tab_price.as_ref()) {
                since
            } else {
                tab_price.as_ref().map(|_| now)
            }
        }
    };

    tx.execute(
        "INSERT INTO stashes(id, account, name, stash_type, league, public, source, first_seen,
                             last_seen, delisted_at, tab_index, price_since)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?7, ?7, NULL, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
             account     = COALESCE(excluded.account, stashes.account),
             name        = COALESCE(excluded.name, stashes.name),
             stash_type  = COALESCE(excluded.stash_type, stashes.stash_type),
             league      = COALESCE(excluded.league, stashes.league),
             public      = excluded.public,
             last_seen   = excluded.last_seen,
             tab_index   = COALESCE(excluded.tab_index, stashes.tab_index),
             price_since = ?10",
        params![
            stash.id,
            stash.account,
            stash.name,
            stash.stash_type,
            stash.league,
            stash.public as i64,
            now,
            origin.source(),
            stash.index,
            price_since,
        ],
    )?;

    let open_items = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id, {EXISTING_COLUMNS}
             FROM items WHERE stash_id = ?1 AND delisted_at IS NULL"
        ))?;
        let map: HashMap<String, ExistingItem> = stmt
            .query_map([&stash.id], |r| {
                Ok((r.get::<_, String>(0)?, existing_item(r, 1)?))
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
            close_item(tx, item_id, stash, origin, change_id, now)?;
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
        // The item's own note always wins over the tab name.
        let note_price = item.note.as_deref().and_then(price::parse_note);
        let (effective, from_tab) = match note_price {
            Some(p) => (Some(p), false),
            None => (tab_price.clone(), tab_price.is_some()),
        };

        // The item may exist in another stash (moved between tabs) or be
        // currently delisted; either way it is not a new listing.
        let previous = match open_items.get(&item.id) {
            Some(e) => Some(e.clone()),
            None => select_item(tx, &item.id)?,
        };
        let changed = previous
            .as_ref()
            .is_some_and(|p| !price::same_price(p.price.as_ref(), effective.as_ref()));

        if previous.as_ref().is_some_and(|p| p.raw == item.raw) {
            touch_item(tx, &item.id, stash, effective.as_ref(), from_tab, now)?;
        } else {
            upsert_item(
                tx,
                item,
                stash,
                effective.as_ref(),
                from_tab,
                origin.source(),
                now,
            )?;
        }
        stats.items_new += usize::from(previous.is_none());

        match &previous {
            None => insert_event(
                tx,
                item.id.as_str(),
                stash,
                origin.listed(),
                &effective,
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
                        origin.relisted(),
                        &effective,
                        None,
                        change_id,
                        now,
                    )?;
                }
                if changed {
                    stats.price_changes += 1;
                    insert_event(
                        tx,
                        item.id.as_str(),
                        stash,
                        "price_changed",
                        &effective,
                        Some(prev),
                        change_id,
                        now,
                    )?;
                }
            }
        }
        if changed {
            tx.execute(
                "UPDATE items SET price_since = ?2 WHERE id = ?1",
                params![item.id, now],
            )?;
        }
    }

    // Anything we had open for this stash that the new snapshot does not
    // contain was removed or sold.
    for item_id in open_items.keys() {
        if !incoming.contains(item_id.as_str()) {
            close_item(tx, item_id, stash, origin, change_id, now)?;
            stats.delisted += 1;
        }
    }

    Ok(())
}

fn select_item(tx: &Transaction<'_>, item_id: &str) -> Result<Option<ExistingItem>> {
    Ok(tx
        .query_row(
            &format!("SELECT {EXISTING_COLUMNS} FROM items WHERE id = ?1"),
            [item_id],
            |r| existing_item(r, 0),
        )
        .optional()?)
}

/// An item seen again unchanged: only where it is, that it is live, and its
/// effective price (the tab name may have changed) need writing. Leaving the
/// text columns alone keeps the FTS update trigger quiet.
fn touch_item(
    tx: &Transaction<'_>,
    item_id: &str,
    stash: &Stash,
    effective: Option<&Price>,
    from_tab: bool,
    now: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE items SET stash_id = ?2, last_seen = ?3, delisted_at = NULL, price_amount = ?4,
                          price_currency = ?5, price_type = ?6, price_from_tab = ?7
         WHERE id = ?1",
        params![
            item_id,
            stash.id,
            now,
            effective.map(|p| p.amount),
            effective.map(|p| p.currency.as_str()),
            effective.map(|p| p.kind.as_str()),
            from_tab as i64,
        ],
    )?;
    Ok(())
}

fn upsert_item(
    tx: &Transaction<'_>,
    item: &crate::model::Item,
    stash: &Stash,
    effective: Option<&Price>,
    from_tab: bool,
    source: &str,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO items(
            id, stash_id, league, account, name, type_line, base_type, rarity, frame_type, ilvl,
            stack_size, sockets, links, corrupted, identified, icon, note, price_amount,
            price_currency, price_type, mods_text, gem_level, gem_quality, map_tier, x, y, w, h,
            source, first_seen, last_seen, delisted_at, raw, price_from_tab, price_since,
            influences, fractured, synthesised, prefixes, suffixes, frame_type_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?30, NULL, ?31,
                 ?32, ?30, ?33, ?34, ?35, ?36, ?37, ?38)
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
            gem_level      = excluded.gem_level,
            gem_quality    = excluded.gem_quality,
            map_tier       = excluded.map_tier,
            x              = excluded.x,
            y              = excluded.y,
            w              = excluded.w,
            h              = excluded.h,
            source         = excluded.source,
            last_seen      = excluded.last_seen,
            delisted_at    = NULL,
            raw            = excluded.raw,
            price_from_tab = excluded.price_from_tab,
            influences     = excluded.influences,
            fractured      = excluded.fractured,
            synthesised    = excluded.synthesised,
            prefixes       = excluded.prefixes,
            suffixes       = excluded.suffixes,
            frame_type_id  = excluded.frame_type_id",
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
            effective.map(|p| p.amount),
            effective.map(|p| p.currency.as_str()),
            effective.map(|p| p.kind.as_str()),
            item.mods_text,
            item.gem_level,
            item.gem_quality,
            item.map_tier,
            item.x,
            item.y,
            item.w,
            item.h,
            source,
            now,
            item.raw,
            from_tab as i64,
            item.influences,
            item.fractured as i64,
            item.synthesised as i64,
            item.prefixes,
            item.suffixes,
            item.frame_type_id,
        ],
    )?;
    Ok(())
}

fn close_item(
    tx: &Transaction<'_>,
    item_id: &str,
    stash: &Stash,
    origin: Origin,
    change_id: &str,
    now: i64,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE items SET delisted_at = ?2 WHERE id = ?1 AND delisted_at IS NULL",
        params![item_id, now],
    )?;
    if changed > 0 {
        let old = tx
            .query_row(
                &format!("SELECT {EXISTING_COLUMNS} FROM items WHERE id = ?1"),
                [item_id],
                |r| existing_item(r, 0),
            )
            .optional()?
            .and_then(|e| e.price);
        insert_event(
            tx,
            item_id,
            stash,
            origin.delisted(),
            &old,
            None,
            change_id,
            now,
        )?;
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
            previous.and_then(|p| p.price.as_ref()).map(|p| p.amount),
            previous
                .and_then(|p| p.price.as_ref())
                .map(|p| p.currency.as_str()),
            now,
            change_id,
        ],
    )?;
    Ok(())
}

/// Bring databases created by older versions up to date. `CREATE TABLE IF NOT
/// EXISTS` handles new tables; columns need explicit checks.
fn migrate(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, column: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    };

    for (table, column, decl) in [
        ("stashes", "source", "TEXT NOT NULL DEFAULT 'stream'"),
        ("items", "source", "TEXT NOT NULL DEFAULT 'stream'"),
        ("items", "value_chaos", "REAL"),
        ("items", "value_divine", "REAL"),
        ("items", "valued_at", "INTEGER"),
        ("items", "gem_level", "INTEGER"),
        ("items", "gem_quality", "INTEGER"),
        ("items", "map_tier", "INTEGER"),
        ("items", "x", "INTEGER"),
        ("items", "y", "INTEGER"),
        ("items", "w", "INTEGER"),
        ("items", "h", "INTEGER"),
        ("items", "value_listings", "INTEGER"),
        ("stashes", "tab_index", "INTEGER"),
        ("stashes", "price_since", "INTEGER"),
        ("items", "price_from_tab", "INTEGER NOT NULL DEFAULT 0"),
        ("items", "price_since", "INTEGER"),
        ("items", "influences", "TEXT"),
        ("items", "fractured", "INTEGER NOT NULL DEFAULT 0"),
        ("items", "synthesised", "INTEGER NOT NULL DEFAULT 0"),
        ("items", "prefixes", "INTEGER"),
        ("items", "suffixes", "INTEGER"),
        ("items", "frame_type_id", "TEXT"),
        ("price_ref", "item_class", "INTEGER"),
        ("price_ref", "level_required", "INTEGER"),
        ("price_ref", "trade_id", "TEXT"),
    ] {
        if !has_column(table, column)? {
            conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
                [],
            )?;
            if (table, column) == ("items", "price_since") {
                // Before v3 the decay clock was derived from the event log on
                // every read; store it once.
                conn.execute(
                    "UPDATE items SET price_since = COALESCE(
                        (SELECT MAX(l.at) FROM listings l
                         WHERE l.item_id = items.id AND l.event = 'price_changed'),
                        (SELECT MIN(l.at) FROM listings l
                         WHERE l.item_id = items.id AND l.event IN ('listed', 'dropped')),
                        first_seen)",
                    [],
                )?;
            }
        }
    }
    Ok(())
}
