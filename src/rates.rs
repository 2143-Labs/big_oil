//! Currency conversion for price notes.
//!
//! Notes are hand-typed (`~b/o 3 divine`, `~b/o 40 alch`), while poe.ninja
//! values are chaos. The exchange overview gives a chaos value per currency,
//! keyed here by its full name and by its trade-site id (`divine`,
//! `mirror-shard`), which is what most notes use; aliases cover the rest.

use std::collections::HashMap;

use crate::ninja::PriceLine;

/// Short forms that do not fall out of the full name by string matching.
const ALIASES: &[(&str, &str)] = &[
    ("alch", "Orb of Alchemy"),
    ("alchemy", "Orb of Alchemy"),
    ("alt", "Orb of Alteration"),
    ("alteration", "Orb of Alteration"),
    ("annul", "Orb of Annulment"),
    ("annulment", "Orb of Annulment"),
    ("aug", "Orb of Augmentation"),
    ("augment", "Orb of Augmentation"),
    ("blessed", "Blessed Orb"),
    ("chance", "Orb of Chance"),
    ("c", "Chaos Orb"),
    ("chaos", "Chaos Orb"),
    ("chrom", "Chromatic Orb"),
    ("chrome", "Chromatic Orb"),
    ("div", "Divine Orb"),
    ("divine", "Divine Orb"),
    ("divines", "Divine Orb"),
    ("ex", "Exalted Orb"),
    ("exa", "Exalted Orb"),
    ("exalt", "Exalted Orb"),
    ("exalted", "Exalted Orb"),
    ("exalts", "Exalted Orb"),
    ("fuse", "Orb of Fusing"),
    ("fusing", "Orb of Fusing"),
    ("gcp", "Gemcutter's Prism"),
    ("gemcutter", "Gemcutter's Prism"),
    ("jew", "Jeweller's Orb"),
    ("jeweller", "Jeweller's Orb"),
    ("mirror", "Mirror of Kalandra"),
    ("regal", "Regal Orb"),
    ("scour", "Orb of Scouring"),
    ("scouring", "Orb of Scouring"),
    ("trans", "Orb of Transmutation"),
    ("transmute", "Orb of Transmutation"),
    ("transmutation", "Orb of Transmutation"),
    ("vaal", "Vaal Orb"),
];

fn normalise(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Chaos per unit of each currency, keyed by normalised name.
#[derive(Debug, Clone, Default)]
pub struct Rates {
    chaos_per: HashMap<String, f64>,
    /// How many rates came from the snapshot, for diagnostics.
    pub entries: usize,
}

impl Rates {
    /// Build from stored economy lines; only the Currency category is used.
    pub fn from_lines(lines: &[PriceLine]) -> Rates {
        let mut chaos_per = HashMap::new();
        for line in lines
            .iter()
            .filter(|l| l.category.eq_ignore_ascii_case("currency"))
        {
            if !line.chaos.is_finite() || line.chaos <= 0.0 {
                continue;
            }
            for key in [&line.name, &line.trade_id]
                .into_iter()
                .flatten()
                .map(|k| normalise(k))
                .filter(|k| !k.is_empty())
            {
                // A currency can appear more than once; keep the first sane value.
                chaos_per.entry(key).or_insert(line.chaos);
            }
        }
        Rates {
            entries: chaos_per.len(),
            chaos_per,
        }
    }

    /// Chaos value of one unit of `token` (as typed in an item note).
    pub fn chaos_per_unit(&self, token: &str) -> Option<f64> {
        let key = normalise(token);
        if key.is_empty() {
            return None;
        }
        // The unit of account never depends on the snapshot.
        if matches!(key.as_str(), "c" | "chaos" | "chaosorb") {
            return Some(1.0);
        }
        // Alias first, then the name or trade id as typed. No substring
        // guessing: "shard" must not quietly become a Mirror Shard.
        ALIASES
            .iter()
            .filter(|(short, _)| *short == key)
            .map(|(_, full)| normalise(full))
            .chain(std::iter::once(key.clone()))
            .find_map(|target| self.chaos_per.get(&target).copied())
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, chaos: f64) -> PriceLine {
        PriceLine {
            league: "Standard".into(),
            category: "Currency".into(),
            name: Some(name.into()),
            chaos,
            ..Default::default()
        }
    }

    fn rates() -> Rates {
        Rates::from_lines(&[
            line("Chaos Orb", 1.0),
            line("Divine Orb", 420.0),
            line("Orb of Alchemy", 0.2),
            line("Mirror of Kalandra", 950_000.0),
            line("Exalted Orb", 12.0),
            PriceLine {
                trade_id: Some("mirror-shard".into()),
                ..line("Mirror Shard", 47_000.0)
            },
        ])
    }

    #[test]
    fn chaos_is_always_one() {
        assert_eq!(rates().chaos_per_unit("chaos"), Some(1.0));
        let empty = Rates::from_lines(&[]);
        assert_eq!(empty.chaos_per_unit("c"), Some(1.0));
        assert_eq!(empty.chaos_per_unit("Chaos Orb"), Some(1.0));
    }

    #[test]
    fn resolves_short_forms() {
        let r = rates();
        assert_eq!(r.chaos_per_unit("divine"), Some(420.0));
        assert_eq!(r.chaos_per_unit("alch"), Some(0.2));
        assert_eq!(r.chaos_per_unit("alchemy"), Some(0.2));
        assert_eq!(r.chaos_per_unit("mirror"), Some(950_000.0));
        assert_eq!(r.chaos_per_unit("exalted"), Some(12.0));
        // Case and punctuation are irrelevant.
        assert_eq!(r.chaos_per_unit("Divine"), Some(420.0));
        assert_eq!(r.chaos_per_unit("div"), Some(420.0));
    }

    #[test]
    fn resolves_trade_ids() {
        assert_eq!(rates().chaos_per_unit("mirror-shard"), Some(47_000.0));
    }

    #[test]
    fn unknown_currencies_stay_unknown() {
        let r = rates();
        assert_eq!(r.chaos_per_unit("weirdbuck"), None);
        assert_eq!(r.chaos_per_unit(""), None);
    }

    #[test]
    fn ambiguous_substrings_do_not_match() {
        // "orb" alone would match several currencies, "shard" a Mirror Shard.
        assert_eq!(rates().chaos_per_unit("orb"), None);
        assert_eq!(rates().chaos_per_unit("shard"), None);
    }
}
