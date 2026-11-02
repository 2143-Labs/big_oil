//! Listed price against market value, and which tab a command means.
//!
//! Every report uses one rule: an item is underpriced when its market value is
//! at least [`Rule::ratio`] times its listed price and worth at least
//! [`Rule::min_value_chaos`]. Such an item sells the moment someone sees it, at
//! the listed price.

use std::fmt;

use anyhow::{Result, bail};

use crate::rates::Rates;
use crate::render;
use crate::store::ItemRow;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    /// Flag when value ≥ listed × `ratio`.
    pub ratio: f64,
    /// Ignore items worth less than this many chaos.
    pub min_value_chaos: f64,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            ratio: 1.5,
            min_value_chaos: 5.0,
        }
    }
}

/// Market value against listed price, both in chaos per unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    pub value: f64,
    pub listed: f64,
}

/// Units in the stack (1 for anything that does not stack).
pub fn stack(row: &ItemRow) -> f64 {
    match row.stack_size {
        Some(n) if n > 0 => n as f64,
        _ => 1.0,
    }
}

/// The listed price of one unit in chaos, when the currency converts.
pub fn listed_chaos(row: &ItemRow, rates: &Rates) -> Option<f64> {
    let amount = row.price_amount.filter(|a| *a > 0.0)?;
    let per_unit = rates.chaos_per_unit(row.price_currency.as_deref()?)?;
    Some(amount * per_unit)
}

pub fn underpriced(row: &ItemRow, rates: &Rates, rule: &Rule) -> Option<Gap> {
    let value = row
        .value_chaos
        .filter(|v| *v > 0.0 && *v >= rule.min_value_chaos)?;
    let listed = listed_chaos(row, rates)?;
    (value >= listed * rule.ratio).then_some(Gap { value, listed })
}

/// An unidentified unique (or relic/foil) can be anything until identified.
pub fn unidentified_unique(row: &ItemRow) -> bool {
    row.identified == Some(false) && matches!(row.frame_type, Some(3 | 9 | 10))
}

pub fn format_price(amount: f64, currency: &str) -> String {
    format!("{} {currency}", render::num(amount))
}

/// `1 divine`, or `1 divine (tab)` when the price comes from the tab name.
pub fn listed_label(row: &ItemRow) -> Option<String> {
    let label = format_price(
        row.price_amount?,
        row.price_currency.as_deref().unwrap_or_default(),
    );
    Some(if row.price_from_tab {
        format!("{label} (tab)")
    } else {
        label
    })
}

pub fn tab_label(row: &ItemRow) -> String {
    row.stash_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "<unnamed tab>".into())
}

/// `--tab 3` (index) or `--tab dump` (case-insensitive name substring). Priced
/// tabs are renamed on every re-price, so the index is the stable handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabSelector {
    Index(i64),
    Name(String),
}

impl TabSelector {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty tab selector");
        }
        if raw.chars().all(|c| c.is_ascii_digit()) {
            return Ok(TabSelector::Index(raw.parse()?));
        }
        Ok(TabSelector::Name(raw.to_lowercase()))
    }

    pub fn matches_tab(&self, index: Option<i64>, name: &str) -> bool {
        match self {
            TabSelector::Index(i) => index == Some(*i),
            TabSelector::Name(needle) => name.to_lowercase().contains(needle.as_str()),
        }
    }

    pub fn matches(&self, row: &ItemRow) -> bool {
        self.matches_tab(row.tab_index, row.stash_name.as_deref().unwrap_or_default())
    }
}

impl fmt::Display for TabSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TabSelector::Index(i) => write!(f, "tab {i}"),
            TabSelector::Name(name) => write!(f, "\"{name}\""),
        }
    }
}

/// The live rows of the one tab `sel` names; an error when it names none or
/// several.
pub fn resolve_one<'a>(rows: &'a [ItemRow], sel: &TabSelector) -> Result<Vec<&'a ItemRow>> {
    let mut tabs: Vec<&ItemRow> = Vec::new();
    for row in rows
        .iter()
        .filter(|r| r.delisted_at.is_none() && sel.matches(r))
    {
        if !tabs.iter().any(|t| t.stash_id == row.stash_id) {
            tabs.push(row);
        }
    }
    match tabs.as_slice() {
        [] => {
            bail!("no scanned tab matches {sel}; `big_oil stash list` shows tab indexes and names")
        }
        [one] => Ok(rows
            .iter()
            .filter(|r| r.delisted_at.is_none() && r.stash_id == one.stash_id)
            .collect()),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|t| {
                    let index = t.tab_index.map_or_else(|| "?".into(), |i| i.to_string());
                    format!("{index}:{}", tab_label(t))
                })
                .collect();
            bail!(
                "{} tabs match {sel}: {}; pass the tab index instead",
                many.len(),
                list.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ninja::PriceLine;

    fn rates() -> Rates {
        Rates::from_lines(&[PriceLine {
            category: "Currency".into(),
            name: Some("Divine Orb".into()),
            chaos: 400.0,
            ..Default::default()
        }])
    }

    fn row(amount: f64, currency: &str, value: Option<f64>) -> ItemRow {
        ItemRow {
            id: "i".into(),
            stash_id: "s".into(),
            stash_name: Some("Dump".into()),
            tab_index: Some(3),
            price_amount: Some(amount),
            price_currency: Some(currency.into()),
            value_chaos: value,
            ..Default::default()
        }
    }

    #[test]
    fn the_rule_flags_at_exactly_the_ratio() {
        let rule = Rule::default();
        assert_eq!(
            underpriced(&row(100.0, "chaos", Some(150.0)), &rates(), &rule),
            Some(Gap {
                value: 150.0,
                listed: 100.0
            })
        );
        assert!(underpriced(&row(100.0, "chaos", Some(149.9)), &rates(), &rule).is_none());
        assert!(underpriced(&row(1.0, "divine", Some(600.0)), &rates(), &rule).is_some());
    }

    #[test]
    fn cheap_items_are_ignored() {
        let rule = Rule::default();
        assert!(underpriced(&row(1.0, "chaos", Some(4.0)), &rates(), &rule).is_none());
        assert!(underpriced(&row(1.0, "chaos", Some(5.0)), &rates(), &rule).is_some());
        assert!(underpriced(&row(1.0, "chaos", None), &rates(), &rule).is_none());
    }

    #[test]
    fn unknown_currencies_are_never_underpriced() {
        let r = row(1.0, "weirdbuck", Some(1000.0));
        assert!(listed_chaos(&r, &rates()).is_none());
        assert!(underpriced(&r, &rates(), &Rule::default()).is_none());
    }

    #[test]
    fn stacks_and_labels() {
        let mut r = row(1.5, "divine", None);
        assert_eq!(stack(&r), 1.0);
        r.stack_size = Some(20);
        assert_eq!(stack(&r), 20.0);
        assert_eq!(listed_label(&r).as_deref(), Some("1.5 divine"));
        r.price_from_tab = true;
        assert_eq!(listed_label(&r).as_deref(), Some("1.5 divine (tab)"));
        r.stash_name = Some("  ".into());
        assert_eq!(tab_label(&r), "<unnamed tab>");
    }

    #[test]
    fn unidentified_uniques_are_spotted() {
        let mut r = row(1.0, "chaos", None);
        r.identified = Some(false);
        r.frame_type = Some(3);
        assert!(unidentified_unique(&r));
        r.frame_type = Some(2);
        assert!(!unidentified_unique(&r));
    }

    #[test]
    fn selectors_parse_and_match() {
        assert_eq!(TabSelector::parse(" 3 ").unwrap(), TabSelector::Index(3));
        assert_eq!(
            TabSelector::parse("Dump").unwrap(),
            TabSelector::Name("dump".into())
        );
        assert!(TabSelector::parse("  ").is_err());

        let r = row(1.0, "chaos", None);
        assert!(TabSelector::Index(3).matches(&r));
        assert!(!TabSelector::Index(4).matches(&r));
        assert!(TabSelector::Name("um".into()).matches(&r));
        assert!(!TabSelector::Name("sell".into()).matches(&r));
        assert_eq!(TabSelector::Index(3).to_string(), "tab 3");
        assert_eq!(TabSelector::Name("dump".into()).to_string(), "\"dump\"");
    }

    #[test]
    fn resolve_one_needs_exactly_one_tab() {
        let a = row(1.0, "chaos", None);
        let mut b = row(1.0, "chaos", None);
        b.id = "j".into();
        b.stash_id = "t".into();
        b.tab_index = Some(4);
        b.stash_name = Some("Dump 2".into());
        let rows = vec![a, b];

        let err = resolve_one(&rows, &TabSelector::Index(9)).unwrap_err();
        assert!(err.to_string().contains("no scanned tab matches tab 9"));

        let one = resolve_one(&rows, &TabSelector::Index(4)).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "j");

        let err = resolve_one(&rows, &TabSelector::parse("dump").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 tabs match"), "{err}");
        assert!(err.contains("3:Dump") && err.contains("4:Dump 2"), "{err}");
    }
}
