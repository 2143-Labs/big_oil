//! The wire model of one hourly digest, plus the canonical ordering that every
//! consumer depends on.

use serde::Deserialize;
use std::collections::BTreeMap;

/// One hour of exchange data. `next_change_id` is the id of the next hour
/// (`hour + 3600`) until the live edge, where it equals the id that was asked
/// for.
#[derive(Debug, Deserialize)]
pub struct Page {
    pub next_change_id: i64,
    #[serde(default)]
    pub markets: Vec<Market>,
}

/// One market. The value dicts are keyed by metadata id
/// (`Metadata/Items/Currency/...`), so every read goes through [`Market::sides`]
/// rather than by position.
#[derive(Debug, Clone, Deserialize)]
pub struct Market {
    pub league: String,
    pub market_id: String,
    pub market_pair: Vec<String>,
    #[serde(default)]
    pub volume_traded: BTreeMap<String, i64>,
    #[serde(default)]
    pub lowest_stock: BTreeMap<String, i64>,
    #[serde(default)]
    pub highest_stock: BTreeMap<String, i64>,
    #[serde(default)]
    pub lowest_ratio: BTreeMap<String, i64>,
    #[serde(default)]
    pub highest_ratio: BTreeMap<String, i64>,
}

/// Both sides of a market, ordered by metadata id (byte-ascending) with every
/// value already swapped to match.
///
/// `market_pair` arrives in the server's hash order, not alphabetical: the
/// chaos/divine market arrives as `["CurrencyRerollRare", "CurrencyModValues"]`.
/// Deriving anything from position silently gets the price upside down, so
/// position stops here.
#[derive(Debug, Clone, PartialEq)]
pub struct Sides {
    pub a: String,
    pub b: String,
    pub vol_a: i64,
    pub vol_b: i64,
    pub stock_lo_a: i64,
    pub stock_hi_a: i64,
    pub stock_lo_b: i64,
    pub stock_hi_b: i64,
    pub ratio_lo_a: i64,
    pub ratio_hi_a: i64,
    pub ratio_lo_b: i64,
    pub ratio_hi_b: i64,
}

impl Sides {
    /// The stable key for this pair: ascending ids joined by `|`. Built here
    /// rather than copied from the payload, because the payload's `market_id`
    /// follows the same hash order and would give one pair two identities.
    pub fn market_id(&self) -> String {
        format!("{}|{}", self.a, self.b)
    }
}

impl Market {
    /// `None` when the pair is malformed: not two entries, or the same id twice.
    /// The caller counts and warns; a malformed pair is not a whole-page error.
    pub fn sides(&self) -> Option<Sides> {
        let [x, y] = self.market_pair.as_slice() else {
            return None;
        };
        if x == y {
            return None;
        }
        let (a, b) = if x < y {
            (x.clone(), y.clone())
        } else {
            (y.clone(), x.clone())
        };
        let get = |m: &BTreeMap<String, i64>, k: &str| m.get(k).copied().unwrap_or(0);
        Some(Sides {
            vol_a: get(&self.volume_traded, &a),
            vol_b: get(&self.volume_traded, &b),
            stock_lo_a: get(&self.lowest_stock, &a),
            stock_hi_a: get(&self.highest_stock, &a),
            stock_lo_b: get(&self.lowest_stock, &b),
            stock_hi_b: get(&self.highest_stock, &b),
            ratio_lo_a: get(&self.lowest_ratio, &a),
            ratio_hi_a: get(&self.highest_ratio, &a),
            ratio_lo_b: get(&self.lowest_ratio, &b),
            ratio_hi_b: get(&self.highest_ratio, &b),
            a,
            b,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHAOS: &str = "Metadata/Items/Currency/CurrencyRerollRare";
    const DIVINE: &str = "Metadata/Items/Currency/CurrencyModValues";

    fn market(pair: [&str; 2]) -> Market {
        serde_json::from_value(json!({
            "league": "Standard",
            "market_id": pair.join("|"),
            "market_pair": pair,
            "volume_traded": {CHAOS: 5, DIVINE: 1},
            "lowest_ratio": {CHAOS: 850, DIVINE: 1},
            "highest_ratio": {CHAOS: 860, DIVINE: 1},
            "lowest_stock": {CHAOS: 10},
            "highest_stock": {CHAOS: 20},
        }))
        .unwrap()
    }

    #[test]
    fn sides_are_position_independent() {
        let forward = market([CHAOS, DIVINE]).sides().unwrap();
        let reversed = market([DIVINE, CHAOS]).sides().unwrap();
        assert_eq!(forward, reversed);
        assert_eq!(forward.a, DIVINE);
        assert_eq!(forward.b, CHAOS);
        assert_eq!(forward.ratio_lo_a, 1);
        assert_eq!(forward.ratio_lo_b, 850);
    }

    #[test]
    fn absent_dict_keys_are_zero() {
        let sides = market([CHAOS, DIVINE]).sides().unwrap();
        // The stock dict only mentions chaos, which is side `b` here.
        assert_eq!(sides.stock_lo_a, 0);
        assert_eq!(sides.stock_hi_a, 0);
        assert_eq!(sides.stock_lo_b, 10);
        assert_eq!(sides.stock_hi_b, 20);
        assert_eq!(sides.vol_b, 5);
        assert_eq!(sides.vol_a, 1);
    }

    #[test]
    fn malformed_pairs_have_no_sides() {
        let one = Market {
            league: "Standard".into(),
            market_id: CHAOS.into(),
            market_pair: vec![CHAOS.into()],
            volume_traded: BTreeMap::new(),
            lowest_stock: BTreeMap::new(),
            highest_stock: BTreeMap::new(),
            lowest_ratio: BTreeMap::new(),
            highest_ratio: BTreeMap::new(),
        };
        assert!(one.sides().is_none());
        assert!(market([CHAOS, CHAOS]).sides().is_none());
    }

    #[test]
    fn market_id_is_canonical() {
        let forward = market([CHAOS, DIVINE]).sides().unwrap().market_id();
        let reversed = market([DIVINE, CHAOS]).sides().unwrap().market_id();
        assert_eq!(forward, reversed);
        assert_eq!(forward, format!("{DIVINE}|{CHAOS}"));
    }
}
