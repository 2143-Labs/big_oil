//! The exchange database: its own file, its own schema version, its own
//! connection. Nothing here is shared with the item store, so the two files can
//! be split apart later without touching either.

use crate::exchange::CHAOS;
use crate::exchange::page::Page;
use crate::exchange::price;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- One row per stored hour: this is the ledger, and its presence is the claim
-- "every market of this hour is in exchange_market".
CREATE TABLE IF NOT EXISTS exchange_hour (
    hour           INTEGER PRIMARY KEY,
    next_change_id INTEGER NOT NULL,
    fetched_at     INTEGER NOT NULL,
    bytes          INTEGER NOT NULL,
    sha256         TEXT NOT NULL,
    markets        INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS exchange_league (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS exchange_pair (
    id        INTEGER PRIMARY KEY,
    market_id TEXT NOT NULL UNIQUE,
    a         TEXT NOT NULL,
    b         TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS exchange_market (
    -- Deferred to COMMIT on purpose: record_hour writes the markets before the
    -- ledger row, so that a ledger row can never outlive its markets. An
    -- immediate constraint would forbid that order.
    hour       INTEGER NOT NULL REFERENCES exchange_hour(hour) DEFERRABLE INITIALLY DEFERRED,
    league_id  INTEGER NOT NULL REFERENCES exchange_league(id),
    pair_id    INTEGER NOT NULL REFERENCES exchange_pair(id),
    vol_a      INTEGER NOT NULL,
    vol_b      INTEGER NOT NULL,
    stock_lo_a INTEGER NOT NULL,
    stock_hi_a INTEGER NOT NULL,
    stock_lo_b INTEGER NOT NULL,
    stock_hi_b INTEGER NOT NULL,
    ratio_lo_a INTEGER NOT NULL,
    ratio_hi_a INTEGER NOT NULL,
    ratio_lo_b INTEGER NOT NULL,
    ratio_hi_b INTEGER NOT NULL,
    PRIMARY KEY (hour, league_id, pair_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS exchange_market_pair ON exchange_market(league_id, pair_id, hour);
"#;

/// One stored hour, as the ledger sees it.
#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub hour: i64,
    pub next_change_id: i64,
    pub fetched_at: i64,
    pub bytes: i64,
    pub sha256: String,
    pub markets: i64,
}

/// A chaos price for one item in one league in one hour.
#[derive(Debug, Clone)]
pub struct PriceRow {
    pub league: String,
    pub item: String,
    pub chaos_mid: f64,
    pub volume: i64,
}

pub struct ExchangeStore {
    conn: Connection,
}

impl ExchangeStore {
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
        // WAL is unavailable on an in-memory database; the item store treats it
        // the same way.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        let mut store = Self { conn };
        store.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(store)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn newest_hour(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(hour) FROM exchange_hour", [], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn has_hour(&self, hour: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM exchange_hour WHERE hour = ?1",
                params![hour],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    pub fn hour_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM exchange_hour", [], |r| r.get(0))?)
    }

    /// When the newest ledger row was written: the liveness signal that a
    /// backfill is still making progress.
    pub fn newest_fetch_at(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(fetched_at) FROM exchange_hour", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten())
    }

    pub fn count_for_hour(&self, hour: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM exchange_market WHERE hour = ?1",
            params![hour],
            |r| r.get(0),
        )?)
    }

    pub fn ledger(&self) -> Result<Vec<LedgerRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT hour, next_change_id, fetched_at, bytes, sha256, markets
             FROM exchange_hour ORDER BY hour",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(LedgerRow {
                hour: r.get(0)?,
                next_change_id: r.get(1)?,
                fetched_at: r.get(2)?,
                bytes: r.get(3)?,
                sha256: r.get(4)?,
                markets: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Markets in one hour with a direct chaos side, one row per item.
    pub fn markets_at(&self, hour: i64) -> Result<Vec<PriceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.name, p.a, p.b, m.ratio_lo_a, m.ratio_hi_a, m.ratio_lo_b,
                    m.ratio_hi_b, m.vol_a, m.vol_b
             FROM exchange_market m
             JOIN exchange_league l ON l.id = m.league_id
             JOIN exchange_pair p ON p.id = m.pair_id
             WHERE m.hour = ?1 AND (p.a = ?2 OR p.b = ?2)
             ORDER BY l.name, p.a, p.b",
        )?;
        let mut rows = stmt.query(params![hour, CHAOS])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let league: String = r.get(0)?;
            let a: String = r.get(1)?;
            let b: String = r.get(2)?;
            let (lo_a, hi_a, lo_b, hi_b): (i64, i64, i64, i64) =
                (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
            let (vol_a, vol_b): (i64, i64) = (r.get(7)?, r.get(8)?);
            let (item, base_lo, base_hi, chaos_lo, chaos_hi, volume) = if a == CHAOS {
                (b, lo_b, hi_b, lo_a, hi_a, vol_b)
            } else {
                (a, lo_a, hi_a, lo_b, hi_b, vol_a)
            };
            let (Some(lo), Some(hi)) = (price(base_lo, chaos_lo), price(base_hi, chaos_hi)) else {
                continue;
            };
            // A low side more than 10x under the high is a stray reported fill,
            // not the market: the Covens league's first hours report a
            // divine/chaos low of 1 chaos against a high of 158. The stored
            // ratios are untouched, so this stays revisitable in SQL.
            let chaos_mid = if hi > lo * 10.0 { hi } else { (lo + hi) / 2.0 };
            out.push(PriceRow {
                chaos_mid,
                league,
                item,
                volume,
            });
        }
        Ok(out)
    }

    pub fn leagues_at(&self, hour: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.name, COUNT(*) FROM exchange_market m
             JOIN exchange_league l ON l.id = m.league_id
             WHERE m.hour = ?1 GROUP BY l.name ORDER BY 2 DESC, 1",
        )?;
        let rows = stmt.query_map(params![hour], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace every row of `hour` in one transaction and record the ledger
    /// entry **last**, so a crash can never leave a ledger row claiming hours of
    /// markets that are not there. Returns the number of market rows written —
    /// what the ledger records, so `verify` catches a market table that drifted
    /// away from it.
    pub fn record_hour(
        &mut self,
        hour: i64,
        page: &Page,
        bytes: i64,
        sha256: &str,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut leagues: HashMap<&str, i64> = HashMap::new();
        let mut pairs: HashMap<String, i64> = HashMap::new();
        let mut stored = 0usize;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO exchange_market
                 (hour, league_id, pair_id, vol_a, vol_b, stock_lo_a, stock_hi_a,
                  stock_lo_b, stock_hi_b, ratio_lo_a, ratio_hi_a, ratio_lo_b, ratio_hi_b)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for market in &page.markets {
                let Some(sides) = market.sides() else {
                    continue;
                };
                let league_id = match leagues.get(market.league.as_str()) {
                    Some(id) => *id,
                    None => {
                        tx.execute(
                            "INSERT INTO exchange_league (name) VALUES (?1)
                             ON CONFLICT(name) DO NOTHING",
                            params![market.league],
                        )?;
                        let id = tx.query_row(
                            "SELECT id FROM exchange_league WHERE name = ?1",
                            params![market.league],
                            |r| r.get(0),
                        )?;
                        leagues.insert(market.league.as_str(), id);
                        id
                    }
                };
                let key = sides.market_id();
                let pair_id = match pairs.get(&key) {
                    Some(id) => *id,
                    None => {
                        tx.execute(
                            "INSERT INTO exchange_pair (market_id, a, b) VALUES (?1, ?2, ?3)
                             ON CONFLICT(market_id) DO NOTHING",
                            params![key, sides.a, sides.b],
                        )?;
                        let id = tx.query_row(
                            "SELECT id FROM exchange_pair WHERE market_id = ?1",
                            params![key],
                            |r| r.get(0),
                        )?;
                        pairs.insert(key, id);
                        id
                    }
                };
                insert.execute(params![
                    hour,
                    league_id,
                    pair_id,
                    sides.vol_a,
                    sides.vol_b,
                    sides.stock_lo_a,
                    sides.stock_hi_a,
                    sides.stock_lo_b,
                    sides.stock_hi_b,
                    sides.ratio_lo_a,
                    sides.ratio_hi_a,
                    sides.ratio_lo_b,
                    sides.ratio_hi_b,
                ])?;
                stored += 1;
            }
        }
        tx.execute(
            "INSERT INTO exchange_hour
             (hour, next_change_id, fetched_at, bytes, sha256, markets)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(hour) DO UPDATE SET
                 next_change_id = excluded.next_change_id,
                 fetched_at     = excluded.fetched_at,
                 bytes          = excluded.bytes,
                 sha256         = excluded.sha256,
                 markets        = excluded.markets",
            params![
                hour,
                page.next_change_id,
                crate::store::unix_now(),
                bytes,
                sha256,
                stored as i64
            ],
        )?;
        tx.commit()?;
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
    const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";

    fn page_with(chaos_lo: i64, chaos_hi: i64) -> Page {
        serde_json::from_value(json!({
            "next_change_id": 1,
            "markets": [{
                "league": "Covens",
                "market_id": format!("{CHAOS}|{DIVINE}"),
                "market_pair": [CHAOS, DIVINE],
                "volume_traded": {CHAOS: 10, DIVINE: 1},
                "lowest_ratio": {CHAOS: chaos_lo, DIVINE: 1},
                "highest_ratio": {CHAOS: chaos_hi, DIVINE: 1},
            }]
        }))
        .unwrap()
    }

    #[test]
    fn a_normal_spread_uses_the_midpoint() {
        let mut store = ExchangeStore::open_memory().unwrap();
        store.record_hour(0, &page_with(130, 150), 1, "h").unwrap();
        let prices = store.markets_at(0).unwrap();
        assert_eq!(prices.len(), 1);
        assert!((prices[0].chaos_mid - 140.0).abs() < 1e-9);
    }

    #[test]
    fn a_stray_low_does_not_drag_the_price_down() {
        let mut store = ExchangeStore::open_memory().unwrap();
        store.record_hour(0, &page_with(1, 158), 1, "h").unwrap();
        let prices = store.markets_at(0).unwrap();
        assert!((prices[0].chaos_mid - 158.0).abs() < 1e-9);
    }

    #[test]
    fn refetching_an_hour_is_idempotent() {
        let mut store = ExchangeStore::open_memory().unwrap();
        store.record_hour(0, &page_with(130, 150), 1, "h1").unwrap();
        store.record_hour(0, &page_with(130, 150), 2, "h2").unwrap();
        assert_eq!(store.hour_count().unwrap(), 1);
        assert_eq!(store.count_for_hour(0).unwrap(), 1);
        assert_eq!(store.ledger().unwrap()[0].sha256, "h2");
    }
}
