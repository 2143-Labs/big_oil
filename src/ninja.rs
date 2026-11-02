//! poe.ninja economy overviews.
//!
//! These endpoints are documented as the supported public surface
//! (<https://poe.ninja/docs/api>), need no auth, and are served with ETags and
//! a ~5 minute cache; the underlying PoE 1 data refreshes roughly every 15
//! minutes. They price *item types*, not individual listings, which is exactly
//! what an offline valuation of your own tabs needs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{agent, urlencode};
use crate::config::app_user_agent;

pub const BASE: &str = "https://poe.ninja";

/// PoE 1 item-overview categories. Fetching all of them is several MB per
/// league, so callers pick a subset.
pub const POE1_ITEM_TYPES: &[&str] = &[
    "UniqueWeapon",
    "UniqueArmour",
    "UniqueAccessory",
    "UniqueFlask",
    "UniqueJewel",
    "UniqueRelic",
    "UniqueTincture",
    "SkillGem",
    "ImbuedGem",
    "ClusterJewel",
    "UniqueMap",
    "BlightedMap",
    "BlightRavagedMap",
    "Map",
    "BaseType",
    "Flask",
    "ForbiddenJewel",
    "ShrineBelt",
    "Invitation",
    "Memory",
    "ValdoMap",
    "Incubator",
    "Corpse",
    "Wombgift",
    "ScryingOrb",
    "Vial",
    "Beast",
    "IncursionTemple",
];

/// Currency-overview categories (currency-like items, one `currencyTypeName`
/// per line).
pub const POE1_CURRENCY_TYPES: &[&str] = &[
    "Currency",
    "Fragment",
    "Tattoo",
    "Omen",
    "Oil",
    "Fossil",
    "Resonator",
    "Scarab",
    "Astrolabe",
    "DeliriumOrb",
    "Artifact",
    "Runegraft",
    "AllflameEmber",
    "DjinnCoin",
    "Ducat",
    "EnshroudingCrystal",
];

/// What a normal refresh fetches: everything a dropped item is likely to be,
/// minus the two largest payloads (`SkillGem` ~3 MB and `BaseType` ~3 MB).
pub const DEFAULT_ITEM_TYPES: &[&str] = &[
    "UniqueWeapon",
    "UniqueArmour",
    "UniqueAccessory",
    "UniqueFlask",
    "UniqueJewel",
    "UniqueRelic",
    "UniqueTincture",
    "ClusterJewel",
    "UniqueMap",
    "BlightedMap",
    "BlightRavagedMap",
];

/// Everything, including gems and every base type.
pub fn all_item_types() -> Vec<&'static str> {
    POE1_ITEM_TYPES.to_vec()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PriceLine {
    pub league: String,
    pub category: String,
    pub name: Option<String>,
    pub base_type: Option<String>,
    pub variant: Option<String>,
    pub links: Option<i64>,
    pub corrupted: Option<bool>,
    pub gem_level: Option<i64>,
    pub gem_quality: Option<i64>,
    pub map_tier: Option<i64>,
    pub chaos: f64,
    pub divine: Option<f64>,
    pub listing_count: Option<i64>,
    pub details_id: Option<String>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct League {
    pub id: String,
    pub name: String,
}

/// What an item looks like to the matcher; extracted from the stash JSON.
#[derive(Debug, Clone, Default)]
pub struct ItemView {
    pub name: Option<String>,
    pub type_line: Option<String>,
    pub base_type: Option<String>,
    pub frame_type: Option<i64>,
    pub corrupted: bool,
    pub links: Option<i64>,
    pub gem_level: Option<i64>,
    pub gem_quality: Option<i64>,
    pub map_tier: Option<i64>,
}

pub enum Overview {
    NotModified,
    Lines {
        lines: Vec<PriceLine>,
        etag: Option<String>,
    },
}

pub struct NinjaClient {
    agent: ureq::Agent,
    base: String,
    user_agent: String,
}

impl NinjaClient {
    pub fn new(contact: &str) -> Self {
        Self::with_base(BASE, contact)
    }

    pub fn with_base(base: impl Into<String>, contact: &str) -> Self {
        NinjaClient {
            agent: agent(std::time::Duration::from_secs(90)),
            base: base.into(),
            user_agent: app_user_agent(contact),
        }
    }

    pub fn leagues(&self) -> Result<Vec<League>> {
        let (status, body, _) = self.get("/poe1/api/economy/leagues", None)?;
        if status != 200 {
            bail!("poe.ninja leagues: HTTP {status}: {}", body.trim());
        }
        serde_json::from_str(&body).context("parsing poe.ninja leagues")
    }

    pub fn item_overview(
        &self,
        league: &str,
        category: &str,
        etag: Option<&str>,
    ) -> Result<Overview> {
        let path = format!(
            "/poe1/api/economy/stash/current/item/overview?league={}&type={}",
            urlencode(league),
            urlencode(category)
        );
        self.overview(league, &path, category, etag, parse_item_lines)
    }

    pub fn currency_overview(
        &self,
        league: &str,
        category: &str,
        etag: Option<&str>,
    ) -> Result<Overview> {
        let path = format!(
            "/poe1/api/economy/stash/current/currency/overview?league={}&type={}",
            urlencode(league),
            urlencode(category)
        );
        self.overview(league, &path, category, etag, parse_currency_lines)
    }

    fn overview(
        &self,
        league: &str,
        path: &str,
        category: &str,
        etag: Option<&str>,
        parse: fn(&str, &str, &Value, i64) -> Vec<PriceLine>,
    ) -> Result<Overview> {
        let (status, body, new_etag) = self.get(path, etag)?;
        match status {
            304 => Ok(Overview::NotModified),
            200 => {
                let value: Value =
                    serde_json::from_str(&body).context("parsing poe.ninja overview")?;
                let fetched_at = crate::store::unix_now();
                Ok(Overview::Lines {
                    lines: parse(league, category, &value, fetched_at),
                    etag: new_etag,
                })
            }
            404 => Ok(Overview::Lines {
                lines: Vec::new(),
                etag: None,
            }),
            other => bail!("poe.ninja {category}: HTTP {other}: {}", body.trim()),
        }
    }

    fn get(&self, path: &str, etag: Option<&str>) -> Result<(u16, String, Option<String>)> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .agent
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/json");
        if let Some(tag) = etag {
            req = req.header("If-None-Match", tag);
        }
        let mut resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let new_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        Ok((status, body, new_etag))
    }
}

fn line_value(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn line_int(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn line_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub fn parse_item_lines(league: &str, category: &str, body: &Value, at: i64) -> Vec<PriceLine> {
    body.get("lines")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| {
                    let chaos = line_value(line, "chaosValue")?;
                    Some(PriceLine {
                        league: league.to_owned(),
                        category: category.to_owned(),
                        name: line_str(line, "name"),
                        base_type: line_str(line, "baseType"),
                        variant: line_str(line, "variant"),
                        links: line_int(line, "links"),
                        corrupted: line.get("corrupted").and_then(Value::as_bool),
                        gem_level: line_int(line, "gemLevel"),
                        gem_quality: line_int(line, "gemQuality"),
                        map_tier: line_int(line, "mapTier"),
                        chaos,
                        divine: line_value(line, "divineValue"),
                        listing_count: line_int(line, "listingCount"),
                        details_id: line_str(line, "detailsId"),
                        fetched_at: at,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_currency_lines(league: &str, category: &str, body: &Value, at: i64) -> Vec<PriceLine> {
    body.get("lines")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(|line| {
                    let chaos = line_value(line, "chaosEquivalent")?;
                    Some(PriceLine {
                        league: league.to_owned(),
                        category: category.to_owned(),
                        name: line_str(line, "currencyTypeName"),
                        base_type: None,
                        variant: None,
                        links: None,
                        corrupted: None,
                        gem_level: None,
                        gem_quality: None,
                        map_tier: None,
                        chaos,
                        divine: line_value(line, "divineEquivalent"),
                        listing_count: line_int(line, "listingCount"),
                        details_id: line_str(line, "detailsId"),
                        fetched_at: at,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pick the best matching overview line for an item.
///
/// Name is the hard filter (unique name, currency name, gem name, base type);
/// ties are broken by the fields that actually move the price: corruption,
/// links, gem level/quality, map tier, base type.
pub fn lookup<'a>(lines: &'a [PriceLine], item: &ItemView) -> Option<&'a PriceLine> {
    let name = item.name.as_deref().unwrap_or("");
    let type_line = item.type_line.as_deref().unwrap_or("");
    let base = item.base_type.as_deref().unwrap_or(type_line);

    let candidates = |key: &str| -> Vec<&PriceLine> {
        if key.is_empty() {
            return Vec::new();
        }
        lines
            .iter()
            .filter(|l| {
                l.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(key))
            })
            .collect()
    };

    // Prefer a unique/currency name, then the base/type line.
    let mut found = candidates(name);
    if found.is_empty() {
        found = candidates(base);
    }
    if found.is_empty() {
        found = candidates(type_line);
    }
    if found.is_empty() {
        return None;
    }

    found.into_iter().max_by(|a, b| {
        score(a, item, base)
            .cmp(&score(b, item, base))
            // Uniques with several variants (e.g. Mageblood flask effects)
            // cannot be told apart from the item JSON, so prefer the most
            // widely listed variant, then the cheaper one.
            .then_with(|| {
                b.listing_count
                    .unwrap_or(0)
                    .cmp(&a.listing_count.unwrap_or(0))
            })
            .then_with(|| {
                b.chaos
                    .partial_cmp(&a.chaos)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    })
}

fn score(line: &PriceLine, item: &ItemView, base: &str) -> i32 {
    let mut score = 0;
    if line
        .base_type
        .as_deref()
        .is_some_and(|b| b.eq_ignore_ascii_case(base))
    {
        score += 2;
    }
    if let Some(corrupted) = line.corrupted {
        score += if corrupted == item.corrupted { 4 } else { -4 };
    }
    if let Some(links) = line.links {
        score += if Some(links) == item.links { 3 } else { -3 };
    }
    if let Some(level) = line.gem_level {
        score += if Some(level) == item.gem_level { 2 } else { -2 };
    }
    if let Some(quality) = line.gem_quality {
        score += if Some(quality) == item.gem_quality {
            2
        } else {
            -2
        };
    }
    if let Some(tier) = line.map_tier {
        score += if Some(tier) == item.map_tier { 1 } else { 0 };
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines() -> Vec<PriceLine> {
        let body = json!({"lines": [
            {"name": "Vaal Fireball", "gemLevel": 20, "gemQuality": 20, "corrupted": true,
             "chaosValue": 2654, "divineValue": 4.0, "listingCount": 40, "detailsId": "vaal-fireball"},
            {"name": "Vaal Fireball", "gemLevel": 1, "gemQuality": null, "corrupted": true,
             "chaosValue": 1.0, "listingCount": 900},
            {"name": "Headhunter", "baseType": "Leather Belt", "chaosValue": 6675,
             "divineValue": 10.0, "listingCount": 1014, "corrupted": false},
            {"name": "Headhunter", "baseType": "Leather Belt", "chaosValue": 9000,
             "listingCount": 3, "corrupted": true}
        ]});
        parse_item_lines("Standard", "Test", &body, 100)
    }

    #[test]
    fn matches_gem_by_level_and_quality() {
        let item = ItemView {
            name: Some("".into()),
            type_line: Some("Vaal Fireball".into()),
            base_type: Some("Vaal Fireball".into()),
            frame_type: Some(4),
            corrupted: true,
            gem_level: Some(20),
            gem_quality: Some(20),
            ..Default::default()
        };
        let lines = lines();
        let line = lookup(&lines, &item).unwrap();
        assert_eq!(line.chaos, 2654.0);
    }

    #[test]
    fn matches_unique_and_prefers_uncorrupted() {
        let item = ItemView {
            name: Some("Headhunter".into()),
            type_line: Some("Leather Belt".into()),
            base_type: Some("Leather Belt".into()),
            frame_type: Some(3),
            corrupted: false,
            ..Default::default()
        };
        let lines = lines();
        let line = lookup(&lines, &item).unwrap();
        assert_eq!(line.chaos, 6675.0);
        assert_eq!(line.listing_count, Some(1014));
    }

    #[test]
    fn currency_lines_parse_by_name() {
        let body = json!({"lines": [
            {"currencyTypeName": "Mirror of Kalandra", "chaosEquivalent": 950480,
             "divineEquivalent": 1420.0, "detailsId": "mirror-of-kalandra"}
        ]});
        let parsed = parse_currency_lines("Standard", "Currency", &body, 100);
        assert_eq!(parsed.len(), 1);
        let item = ItemView {
            type_line: Some("Mirror of Kalandra".into()),
            frame_type: Some(5),
            ..Default::default()
        };
        assert_eq!(lookup(&parsed, &item).unwrap().chaos, 950480.0);
    }

    #[test]
    fn unknown_items_do_not_match() {
        let item = ItemView {
            name: Some("Nobody's Item".into()),
            type_line: Some("Nobody's Base".into()),
            ..Default::default()
        };
        assert!(lookup(&lines(), &item).is_none());
    }
}
