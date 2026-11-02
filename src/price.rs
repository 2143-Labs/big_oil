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

impl Price {
    /// Same amount, currency and kind. Kind counts: `~b/o` → `~price` is a
    /// change a buyer sees.
    pub fn same_as(&self, other: &Price) -> bool {
        (self.amount - other.amount).abs() < 1e-9
            && self.currency.eq_ignore_ascii_case(&other.currency)
            && self.kind == other.kind
    }
}

/// Equality over optional prices: two missing prices are the same.
pub fn same_price(a: Option<&Price>, b: Option<&Price>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.same_as(b),
        _ => false,
    }
}

/// Parse a price note. Returns `None` for free text, "sold", and anything else
/// that is not an amount + currency.
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

    let amount = parse_amount(parts.next()?)?;

    // "3 mirror shard" is the shard, not the mirror.
    let first = parts.next()?.to_ascii_lowercase();
    let currency = match parts.next().map(str::to_ascii_lowercase).as_deref() {
        Some("shard" | "shards") => format!("{first}-shard"),
        _ => first,
    };
    // A note that keeps talking ("~b/o 1 divine for both") is still usable: the
    // amount and currency are unambiguous, the rest is commentary.
    Some(Price {
        kind: kind.to_owned(),
        amount,
        currency,
    })
}

/// `12`, `1.5`, `1,5` (decimal comma), `1,000` (thousands), or `a/b`: a stack
/// price for `b` units, so the note prices one unit at `a / b`.
fn parse_amount(token: &str) -> Option<f64> {
    let value = if let Some((a, b)) = token.split_once('/') {
        let a: f64 = a.parse().ok()?;
        let b: f64 = b.parse().ok()?;
        if !(a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0) {
            return None;
        }
        a / b
    } else if is_decimal_comma(token) {
        token.replace(',', ".").parse().ok()?
    } else {
        token.replace(',', "").parse().ok()?
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Exactly one comma, no dot, and one or two digits after it.
fn is_decimal_comma(token: &str) -> bool {
    if token.contains('.') || token.matches(',').count() != 1 {
        return false;
    }
    token.split_once(',').is_some_and(|(_, frac)| {
        (1..=2).contains(&frac.len()) && frac.chars().all(|c| c.is_ascii_digit())
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
        assert_eq!(parse_note("~b/o 1,5 divine").unwrap().amount, 1.5);
        assert_eq!(parse_note("~b/o 1/2 divine").unwrap().amount, 0.5);
        assert_eq!(parse_note("~price 1/10 chaos").unwrap().amount, 0.1);
        let shard = parse_note("~b/o 3 mirror shard").unwrap();
        assert_eq!(
            (shard.amount, shard.currency.as_str()),
            (3.0, "mirror-shard")
        );
        let both = parse_note("~price 1 divine for both").unwrap();
        assert_eq!((both.amount, both.currency.as_str()), (1.0, "divine"));
    }

    #[test]
    fn rejects_non_prices() {
        for note in [
            "",
            "sold",
            "~b/o amount",
            "10 chaos",
            "~b/o 0 chaos",
            "~b/o -3 chaos",
        ] {
            assert!(parse_note(note).is_none(), "{note:?} should not parse");
        }
    }
}
