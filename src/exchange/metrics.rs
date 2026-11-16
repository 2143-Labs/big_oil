//! The metric layer: a hand-written exposition body, no metrics crate, no HTTP
//! server framework.
//!
//! Series identity is the metadata id, not a display name: no API maps
//! `Metadata/Items/...` to a name (`/api/trade/data/items` contains zero
//! `Metadata/` ids), and inventing a mapping would make a series' identity move
//! underneath a dashboard.

use crate::exchange::store::ExchangeStore;
use anyhow::Result;
use std::fmt::Write as _;

/// The items exposed as series. Kept to the liquid currency of a trade league
/// so the series count stays around `items × leagues` instead of the ~12k a
/// full per-pair export would create — Mimir has hit its per-tenant series limit
/// before, and breadth belongs in SQL.
pub const METRIC_ITEMS: &[&str] = &[
    "Metadata/Items/Currency/CurrencyModValues", // Divine Orb
    "Metadata/Items/Currency/CurrencyAddModToRare", // Exalted Orb
    "Metadata/Items/Currency/CurrencyDuplicate", // Mirror of Kalandra
    "Metadata/Items/Currency/CurrencyDuplicateShard",
    "Metadata/Items/Currency/CurrencyUpgradeToRare", // Orb of Alchemy
    "Metadata/Items/Currency/CurrencyUpgradeToMagic",
    "Metadata/Items/Currency/CurrencyRerollMagic",
    "Metadata/Items/Currency/CurrencyUpgradeRandomly",
    "Metadata/Items/Currency/CurrencyIdentification",
    "Metadata/Items/Currency/CurrencyRerollSocketNumbers",
    "Metadata/Items/Currency/CurrencyRerollSocketLinks",
    "Metadata/Items/Currency/CurrencyRerollSocketColours",
    "Metadata/Items/Currency/CurrencyPassiveRefund",
    "Metadata/Items/Currency/CurrencyGemQuality",
    "Metadata/Items/Currency/CurrencyArmourQuality",
    "Metadata/Items/Currency/CurrencyWeaponQuality",
    "Metadata/Items/Currency/CurrencyFlaskQuality",
    "Metadata/Items/Currency/CurrencyMapQualityCurrency",
    "Metadata/Items/Currency/CurrencyCorrupt",
];

pub fn render(
    store: &ExchangeStore,
    fetch_errors: u64,
    started_at: i64,
    _now: i64,
) -> Result<String> {
    let newest = store.newest_hour()?;
    let mut out = String::new();
    out.push_str(
        "# HELP poe_exchange_last_hour_timestamp Unix time of the newest stored exchange hour.\n",
    );
    out.push_str("# TYPE poe_exchange_last_hour_timestamp gauge\n");
    let _ = writeln!(
        out,
        "poe_exchange_last_hour_timestamp {}",
        newest.unwrap_or(0)
    );
    out.push_str("# HELP poe_exchange_hours Stored exchange hours (the ledger).\n");
    out.push_str("# TYPE poe_exchange_hours gauge\n");
    let _ = writeln!(out, "poe_exchange_hours {}", store.hour_count()?);
    out.push_str("# HELP poe_exchange_started_at_seconds When this process started.\n");
    out.push_str("# TYPE poe_exchange_started_at_seconds gauge\n");
    let _ = writeln!(out, "poe_exchange_started_at_seconds {started_at}");
    out.push_str("# HELP poe_exchange_fetch_errors_total Failed collection runs.\n");
    out.push_str("# TYPE poe_exchange_fetch_errors_total counter\n");
    let _ = writeln!(out, "poe_exchange_fetch_errors_total {fetch_errors}");

    let Some(newest) = newest else {
        return Ok(out);
    };
    out.push_str("# HELP poe_exchange_markets Markets in the newest stored hour, per league.\n");
    out.push_str("# TYPE poe_exchange_markets gauge\n");
    for (league, markets) in store.leagues_at(newest)? {
        let _ = writeln!(
            out,
            "poe_exchange_markets{{league=\"{}\"}} {markets}",
            escape(&league)
        );
    }

    out.push_str(
        "# HELP poe_exchange_chaos Chaos price of one unit, midpoint of the ratio spread.\n",
    );
    out.push_str("# TYPE poe_exchange_chaos gauge\n");
    out.push_str("# HELP poe_exchange_volume Units traded in the newest stored hour.\n");
    out.push_str("# TYPE poe_exchange_volume gauge\n");
    let mut volumes = String::new();
    for row in store.markets_at(newest)? {
        if !METRIC_ITEMS.contains(&row.item.as_str()) {
            continue;
        }
        let labels = format!(
            "league=\"{}\",item=\"{}\"",
            escape(&row.league),
            escape(&row.item)
        );
        let _ = writeln!(
            out,
            "poe_exchange_chaos{{{labels}}} {}",
            number(row.chaos_mid)
        );
        let _ = writeln!(volumes, "poe_exchange_volume{{{labels}}} {}", row.volume);
    }
    out.push_str(&volumes);
    Ok(out)
}

/// Six significant decimals, so a value does not jitter between scrapes because
/// of float printing.
fn number(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let r = (v * 1e6).round() / 1e6;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// Label values are the only untrusted part of this output.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::page::Page;

    const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
    const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";

    fn store_with_one_hour() -> ExchangeStore {
        let mut store = ExchangeStore::open_memory().unwrap();
        let page: Page = serde_json::from_value(serde_json::json!({
            "next_change_id": 1794603600i64,
            "markets": [{
                "league": "Covens",
                "market_id": format!("{CHAOS}|{DIVINE}"),
                "market_pair": [CHAOS, DIVINE],
                "volume_traded": {CHAOS: 900, DIVINE: 1},
                "lowest_ratio": {CHAOS: 850, DIVINE: 1},
                "highest_ratio": {CHAOS: 850, DIVINE: 1},
                "lowest_stock": {CHAOS: 10},
                "highest_stock": {CHAOS: 20},
            }]
        }))
        .unwrap();
        store.record_hour(1794600000, &page, 1, "hash").unwrap();
        store
    }

    #[test]
    fn renders_the_newest_hour_as_chaos_prices() {
        let store = store_with_one_hour();
        let body = render(&store, 0, 1_794_600_000, 1_794_603_600).unwrap();
        assert!(body.contains("poe_exchange_last_hour_timestamp 1794600000\n"));
        assert!(body.contains("poe_exchange_hours 1\n"));
        assert!(body.contains("poe_exchange_markets{league=\"Covens\"} 1\n"));
        assert!(body.contains(&format!(
            "poe_exchange_chaos{{league=\"Covens\",item=\"{DIVINE}\"}} 850\n"
        )));
        // The item's own volume, not the chaos side's.
        assert!(body.contains(&format!(
            "poe_exchange_volume{{league=\"Covens\",item=\"{DIVINE}\"}} 1\n"
        )));
    }

    #[test]
    fn an_empty_store_still_exposes_ingest_health() {
        let store = ExchangeStore::open_memory().unwrap();
        let body = render(&store, 7, 42, 0).unwrap();
        assert!(body.contains("poe_exchange_last_hour_timestamp 0\n"));
        assert!(body.contains("poe_exchange_fetch_errors_total 7\n"));
        assert!(!body.contains("poe_exchange_chaos{"));
    }
}
