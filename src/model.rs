//! Types for the Public Stash API (`GET /public-stash-tabs`).
//!
//! GGG's schema drifts between patches, so the page envelope is typed while
//! each item is kept as raw JSON: we store the exact bytes we received and pull
//! the handful of fields we index out of it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StashPage {
    pub next_change_id: String,
    #[serde(default)]
    pub stashes: Vec<PublicStashChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublicStashChange {
    pub id: String,
    #[serde(default)]
    pub public: bool,
    #[serde(default, rename = "accountName")]
    pub account_name: Option<String>,
    #[serde(default)]
    pub stash: Option<String>,
    #[serde(default, rename = "stashType")]
    pub stash_type: Option<String>,
    #[serde(default)]
    pub league: Option<String>,
    /// Absent entirely when `public` is false.
    #[serde(default)]
    pub items: Vec<Value>,
}

impl StashPage {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("parsing public stash page JSON")
    }
}

/// A single stash in the stream. `public == false` is the "unlisted" event and
/// carries nothing but `id`/`public`, so every other field is optional.
#[derive(Debug, Clone)]
pub struct Stash {
    pub id: String,
    pub public: bool,
    pub account: Option<String>,
    pub name: Option<String>,
    pub stash_type: Option<String>,
    pub league: Option<String>,
    /// Position in the account's tab list (own tabs only).
    pub index: Option<i64>,
    /// Items omitted unless `public`.
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub id: String,
    pub name: Option<String>,
    pub type_line: Option<String>,
    pub base_type: Option<String>,
    pub rarity: Option<String>,
    pub frame_type: Option<i64>,
    pub ilvl: Option<i64>,
    pub stack_size: Option<i64>,
    pub sockets: Option<String>,
    pub links: Option<i64>,
    pub corrupted: bool,
    pub identified: Option<bool>,
    pub gem_level: Option<i64>,
    pub gem_quality: Option<i64>,
    pub map_tier: Option<i64>,
    /// Stash position, so a report can say where to find the item.
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub w: Option<i64>,
    pub h: Option<i64>,
    pub icon: Option<String>,
    /// Free text the seller typed; price notes (`~b/o 1 divine`) live here.
    pub note: Option<String>,
    /// All mod lines (implicit/explicit/enchant/crafted/fractured/utility)
    /// flattened, for full-text search.
    pub mods_text: String,
    /// Influences set to `true`, lower-cased, sorted, comma-joined.
    pub influences: Option<String>,
    pub fractured: bool,
    pub synthesised: bool,
    /// Affix counts from `extended`, when the API sends them.
    pub prefixes: Option<i64>,
    pub suffixes: Option<i64>,
    /// The string frame type; stored only (matching uses `frame_type`).
    pub frame_type_id: Option<String>,
    /// Item JSON re-serialised from the parsed value (values preserved, key
    /// order not).
    pub raw: String,
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn int_field(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

/// Mod arrays appear in two shapes depending on endpoint/version: plain strings
/// (`["+20 to maximum Life"]`) and the extended objects returned by the trade
/// and stash APIs today (`[{"description": "...", "hash": "...", "mods": [...]}]`).
fn mod_lines(v: &Value, key: &str, out: &mut Vec<String>) {
    let Some(lines) = v.get(key).and_then(Value::as_array) else {
        return;
    };
    for line in lines {
        if let Some(s) = line.as_str() {
            out.push(s.to_owned());
        } else if let Some(desc) = line.get("description").and_then(Value::as_str) {
            out.push(desc.to_owned());
        }
    }
}

impl Item {
    pub fn from_value(v: &Value) -> Item {
        let mut mods = Vec::new();
        for key in [
            "enchantMods",
            "implicitMods",
            "explicitMods",
            "craftedMods",
            "fracturedMods",
            "utilityMods",
            "scourgeMods",
            "crucibleMods",
        ] {
            mod_lines(v, key, &mut mods);
        }
        // `extended.text` is the base64-encoded clipboard block (whole item,
        // flavour text included), not a mod list — indexing it would poison
        // full-text search.

        let (sockets, links) = parse_sockets(v);
        let extended = v.get("extended");

        Item {
            id: str_field(v, "id").unwrap_or_default(),
            name: str_field(v, "name"),
            type_line: str_field(v, "typeLine"),
            base_type: str_field(v, "baseType"),
            rarity: str_field(v, "rarity"),
            frame_type: int_field(v, "frameType"),
            ilvl: int_field(v, "ilvl"),
            stack_size: int_field(v, "stackSize"),
            sockets,
            links,
            corrupted: bool_field(v, "corrupted").unwrap_or(false),
            identified: bool_field(v, "identified"),
            gem_level: property_number(v, "Level"),
            gem_quality: property_number(v, "Quality"),
            map_tier: property_number(v, "Map Tier"),
            x: int_field(v, "x"),
            y: int_field(v, "y"),
            w: int_field(v, "w"),
            h: int_field(v, "h"),
            icon: str_field(v, "icon"),
            note: str_field(v, "note"),
            mods_text: mods.join("\n"),
            influences: parse_influences(v),
            fractured: bool_field(v, "fractured").unwrap_or(false),
            synthesised: bool_field(v, "synthesised").unwrap_or(false),
            prefixes: extended.and_then(|e| int_field(e, "prefixes")),
            suffixes: extended.and_then(|e| int_field(e, "suffixes")),
            frame_type_id: str_field(v, "frameTypeId"),
            raw: v.to_string(),
        }
    }

    /// Some entries in the stream have no id (e.g. items inside a "removed"
    /// placeholder). Those cannot be tracked and are skipped.
    pub fn is_trackable(&self) -> bool {
        !self.id.is_empty()
    }
}

/// `{"influences": {"shaper": true, "elder": true}}` → `elder,shaper`.
fn parse_influences(v: &Value) -> Option<String> {
    let mut names: Vec<String> = v
        .get("influences")?
        .as_object()?
        .iter()
        .filter(|(_, set)| set.as_bool() == Some(true))
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    Some(names.join(","))
}

/// Pull a numeric value out of the rendered `properties` block, which is where
/// gem level/quality and map tier live (`{"name":"Level","values":[["20",0]]}`).
/// Only the leading digits count: `20 (Max)` is 20, `+23%` is 23.
fn property_number(v: &Value, key: &str) -> Option<i64> {
    let raw = v
        .get("properties")?
        .as_array()?
        .iter()
        .find(|p| {
            p.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(key))
        })?
        .get("values")?
        .get(0)?
        .get(0)?
        .as_str()?;
    let raw = raw.trim();
    let raw = raw.strip_prefix('+').unwrap_or(raw);
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `sockets` is `[{group, attr, sColour}, ...]`; `links` is the largest group.
fn parse_sockets(v: &Value) -> (Option<String>, Option<i64>) {
    let Some(arr) = v.get("sockets").and_then(Value::as_array) else {
        return (None, None);
    };
    let mut colours = Vec::with_capacity(arr.len());
    let mut groups: Vec<i64> = Vec::new();
    for socket in arr {
        if let Some(c) = socket.get("sColour").and_then(Value::as_str) {
            colours.push(c.to_owned());
        }
        if let Some(g) = socket.get("group").and_then(Value::as_i64) {
            groups.push(g);
        }
    }
    let sockets = if colours.is_empty() {
        None
    } else {
        Some(colours.join("-"))
    };
    let mut counts: Vec<(i64, i64)> = Vec::new();
    for g in groups {
        match counts.iter_mut().find(|(id, _)| *id == g) {
            Some((_, n)) => *n += 1,
            None => counts.push((g, 1)),
        }
    }
    let links = counts.iter().map(|(_, n)| *n).max();
    (sockets, links)
}

impl PublicStashChange {
    pub fn into_stash(self) -> Stash {
        let items = self
            .items
            .iter()
            .map(Item::from_value)
            .filter(Item::is_trackable)
            .collect();
        Stash {
            id: self.id,
            public: self.public,
            account: self.account_name,
            name: self.stash,
            stash_type: self.stash_type,
            league: self.league,
            index: None,
            items,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sockets_and_links() {
        let v = json!({
            "id": "abc",
            "sockets": [
                {"group": 0, "attr": "S", "sColour": "R"},
                {"group": 0, "attr": "S", "sColour": "G"},
                {"group": 0, "attr": "S", "sColour": "B"},
                {"group": 1, "attr": "D", "sColour": "B"}
            ]
        });
        let item = Item::from_value(&v);
        assert_eq!(item.sockets.as_deref(), Some("R-G-B-B"));
        assert_eq!(item.links, Some(3));
    }

    #[test]
    fn collects_mods_from_every_bucket() {
        let v = json!({
            "id": "abc",
            "implicitMods": ["+20 to maximum Life"],
            "explicitMods": ["+10% to Fire Resistance"],
            "craftedMods": ["+5 to Dexterity"],
            "extended": {"text": "SXRlbSBDbGFzczogQmVsdHM="}
        });
        let item = Item::from_value(&v);
        assert!(item.mods_text.contains("maximum Life"));
        assert!(item.mods_text.contains("Fire Resistance"));
        assert!(item.mods_text.contains("Dexterity"));
        assert!(
            !item.mods_text.contains("SXRlbS"),
            "base64 clipboard block must not reach the search index"
        );
    }

    #[test]
    fn collects_object_form_mods() {
        // Shape returned by the trade/stash APIs today.
        let v = json!({
            "id": "abc",
            "explicitMods": [
                {"description": "+1 to Level of Socketed Minion Gems", "hash": "stat.explicit.stat_3604946673"},
                {"description": "+27% to Fire Resistance", "hash": "stat.explicit.stat_3372524247"}
            ]
        });
        let item = Item::from_value(&v);
        assert!(item.mods_text.contains("Socketed Minion Gems"));
        assert!(item.mods_text.contains("Fire Resistance"));
    }

    #[test]
    fn reads_gem_and_map_properties() {
        let gem = json!({
            "id": "g",
            "properties": [
                {"name": "Level", "values": [["20", 0]]},
                {"name": "Quality", "values": [["+23%", 1]]}
            ]
        });
        let item = Item::from_value(&gem);
        assert_eq!(item.gem_level, Some(20));
        assert_eq!(item.gem_quality, Some(23));

        let map = json!({
            "id": "m",
            "properties": [{"name": "Map Tier", "values": [["16", 0]]}]
        });
        assert_eq!(Item::from_value(&map).map_tier, Some(16));

        let maxed = json!({
            "id": "g2",
            "properties": [{"name": "Level", "values": [["20 (Max)", 0]]}]
        });
        assert_eq!(Item::from_value(&maxed).gem_level, Some(20));
    }

    #[test]
    fn reads_influences_and_affix_counts() {
        let v = json!({
            "id": "r",
            "influences": {"shaper": true, "elder": true},
            "extended": {"prefixes": 2, "suffixes": 1}
        });
        let item = Item::from_value(&v);
        assert_eq!(item.influences.as_deref(), Some("elder,shaper"));
        assert_eq!(item.prefixes, Some(2));
        assert_eq!(item.suffixes, Some(1));
        assert!(Item::from_value(&json!({"id": "x"})).influences.is_none());
    }

    #[test]
    fn unlisted_stash_parses_without_items() {
        let page: StashPage = serde_json::from_str(
            r#"{"next_change_id":"x","stashes":[{"id":"s1","public":false}]}"#,
        )
        .unwrap();
        let stash = page.stashes.into_iter().next().unwrap().into_stash();
        assert!(!stash.public);
        assert!(stash.items.is_empty());
        assert!(stash.account.is_none());
    }
}
