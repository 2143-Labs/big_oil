//! Price notes.
//!
//! Sellers price items by typing a note into the item ("~b/o 12 chaos"), which
//! the Public Stash API returns as `Item.note`. There is no structured price
//! field, so this is what we get.

#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    /// `b/o`, `gb/o`, `c/o` or `price`.
    pub kind: String,
    pub amount: f64,
    pub currency: String,
}

/// Parse a price note. Returns `None` for free text, "sold", bulk-only notes,
/// and anything else that is not a single amount + currency.
pub fn parse_note(note: &str) -> Option<Price> {
    let note = note.trim();
    let note = note.strip_prefix('~').unwrap_or(note).trim_start();

    let mut parts = note.split_whitespace();
    let prefix = parts.next()?;
    let kind = match prefix.to_ascii_lowercase().as_str() {
        "b/o" => "b/o",
        "gb/o" => "gb/o",
        "c/o" => "c/o",
        "price" => "price",
        _ => return None,
    };

    let amount = parts.next()?.replace(',', "");
    // "1/2 divine" and friends are not a single price.
    if amount.contains('/') {
        return None;
    }
    let amount: f64 = amount.parse().ok()?;
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }

    let currency = parts.next()?.to_ascii_lowercase();
    // A note that keeps talking ("~b/o 1 divine for both") is still usable: the
    // amount and currency are unambiguous, the rest is commentary.
    Some(Price {
        kind: kind.to_owned(),
        amount,
        currency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_notes() {
        assert_eq!(
            parse_note("~b/o 12 chaos"),
            Some(Price {
                kind: "b/o".into(),
                amount: 12.0,
                currency: "chaos".into()
            })
        );
        assert_eq!(parse_note("~price 1 divine").unwrap().amount, 1.0);
        // Notes are typed by hand, so case and extra commentary happen.
        assert_eq!(
            parse_note("~B/O 250 Divine offer only").unwrap().currency,
            "divine"
        );
        assert_eq!(parse_note("~gb/o 1.5 mirror").unwrap().amount, 1.5);
        assert_eq!(parse_note("~b/o 1,000 chaos").unwrap().amount, 1000.0);
    }

    #[test]
    fn rejects_non_prices() {
        for note in [
            "",
            "sold",
            "~b/o 1/2 divine",
            "~b/o amount",
            "10 chaos",
            "~b/o 0 chaos",
            "~b/o -3 chaos",
        ] {
            assert!(parse_note(note).is_none(), "{note:?} should not parse");
        }
    }
}
