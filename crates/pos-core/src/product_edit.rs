//! Adding and editing one product: what the form collects, how a price follows from the cost and
//! the product group's markup, what is checked before saving, and what the user should be warned
//! about first. No UI or database in here.
//!
//! Only standalone products (not in an alias/variant group, not a case) can be edited so far;
//! `ProductDetail::membership` tells the caller which it is.

use thiserror::Error;

use crate::{product::Membership, repo::StorageError};

/// Highest cost the form accepts, in cents (the Python form allows up to 999,999.00).
pub const MAX_COST_CENTS: i64 = 99_999_900;

/// A product group such as "MEATS" with its markup (`0.35` = 35 %). Stored in the database as
/// `groups.profit_margin`, but it is a markup on cost: price = cost x (1 + markup).
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub markup: f64,
}

/// A product as stored: its own columns, not the group-resolved values the list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductDetail {
    pub id: i64,
    pub barcode: String,
    pub name: String,
    pub group_id: Option<i64>,
    pub cost_cents: i64,
    pub price_cents: i64,
    pub gct: bool,
    pub membership: Membership,
}

/// What the form collects (before `normalized`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductDraft {
    pub barcode: String,
    pub name: String,
    pub cost_cents: i64,
    pub group_id: Option<i64>,
    pub gct: bool,
}

/// A draft after validation, with the price worked out. This is what gets stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductWrite {
    pub barcode: String,
    pub name: String,
    pub cost_cents: i64,
    pub price_cents: i64,
    pub group_id: Option<i64>,
    pub gct: bool,
}

/// Something the user should confirm before an edit is saved. Typed here so every front end
/// asks the same question; the window only decides how to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductWarning {
    /// Shelf labels, price tags and scanners still carry the old barcode.
    BarcodeChange { old: String, new: String },
}

impl ProductWarning {
    pub fn message(&self) -> String {
        match self {
            ProductWarning::BarcodeChange { old, new } => format!(
                "Change the barcode from {old} to {new}?\n\nLabels, price tags and scanners that still use \
                 the old barcode will no longer find this product."
            ),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProductError {
    /// A message to show next to the form as it is.
    #[error("{0}")]
    Invalid(String),
    #[error("Another product already uses barcode {0}.")]
    DuplicateBarcode(String),
    #[error("That product no longer exists.")]
    NotFound,
    #[error("{0}")]
    Storage(#[from] StorageError),
}

fn invalid<T>(msg: &str) -> Result<T, ProductError> {
    Err(ProductError::Invalid(msg.to_string()))
}

/// `price = cost x (1 + markup)`, rounded to the cent. No markup, or no cost, leaves the cost as
/// the price (like the Python form).
pub fn price_from_markup(cost_cents: i64, markup: f64) -> i64 {
    if cost_cents <= 0 || !markup.is_finite() || markup <= 0.0 {
        return cost_cents;
    }
    (cost_cents as f64 * (1.0 + markup)).round() as i64
}

/// Text typed in the Cost box -> cents, without going through floating point.
/// Accepts `12`, `12.5`, `12.50`, `.5`, `1,200.00`, `$3.99`; at most two decimals.
pub fn parse_cost(text: &str) -> Result<i64, ProductError> {
    let t: String = text.trim().trim_start_matches('$').chars().filter(|c| *c != ',').collect();
    if t.is_empty() {
        return Ok(0); // an empty cost is 0.00, like the Python form
    }
    let (whole, frac) = match t.split_once('.') {
        Some((w, f)) => (w, f),
        None => (t.as_str(), ""),
    };
    let digits_only = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if !digits_only(whole) || !digits_only(frac) || (whole.is_empty() && frac.is_empty()) {
        return invalid("Enter a valid cost.");
    }
    if frac.len() > 2 {
        return invalid("Cost can have at most 2 decimal places.");
    }
    let whole_cents: i64 = if whole.is_empty() { 0 } else { whole.parse::<i64>().map_err(|_| ProductError::Invalid("Enter a valid cost.".into()))? };
    let frac_cents: i64 = format!("{frac:0<2}").parse().unwrap_or(0);
    let cents = whole_cents.checked_mul(100).and_then(|c| c.checked_add(frac_cents));
    match cents {
        Some(c) if c <= MAX_COST_CENTS => Ok(c),
        _ => invalid("Cost can be at most 999,999.00."),
    }
}

impl ProductDraft {
    /// Trims and upper-cases barcode and name (the database stores them that way).
    pub fn normalized(&self) -> ProductDraft {
        ProductDraft {
            barcode: self.barcode.trim().to_uppercase(),
            name: self.name.trim().to_uppercase(),
            ..self.clone()
        }
    }

    /// Checks the draft and works out the price from the group's markup (`None` = no group).
    pub fn into_write(&self, markup: Option<f64>) -> Result<ProductWrite, ProductError> {
        let d = self.normalized();
        if d.barcode.is_empty() || d.name.is_empty() {
            return invalid("Barcode and Name are required.");
        }
        if d.cost_cents < 0 || d.cost_cents > MAX_COST_CENTS {
            return invalid("Enter a valid cost.");
        }
        Ok(ProductWrite {
            price_cents: price_from_markup(d.cost_cents, markup.unwrap_or(0.0)),
            barcode: d.barcode,
            name: d.name,
            cost_cents: d.cost_cents,
            group_id: d.group_id,
            gct: d.gct,
        })
    }
}

/// What to confirm before saving `draft` over `original`.
pub fn warnings_for_edit(original: &ProductDetail, draft: &ProductDraft) -> Vec<ProductWarning> {
    let new = draft.barcode.trim().to_uppercase();
    if !new.is_empty() && new != original.barcode.trim().to_uppercase() {
        vec![ProductWarning::BarcodeChange { old: original.barcode.clone(), new }]
    } else {
        Vec::new()
    }
}

/// "= $1.00 x (1 + 25%) = $1.25", or "" when there is no markup to explain.
pub fn price_hint(cost_cents: i64, markup: f64, symbol: &str) -> String {
    if cost_cents <= 0 || !markup.is_finite() || markup <= 0.0 {
        return String::new();
    }
    use crate::money::format_money;
    format!(
        "= {} \u{d7} (1 + {:.0}%) = {}",
        format_money(cost_cents, symbol),
        markup * 100.0,
        format_money(price_from_markup(cost_cents, markup), symbol)
    )
}

/// How far a saved edit reached beyond the product itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// Linked case products whose cost and price were recalculated (the cost changed).
    pub cases_updated: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(barcode: &str, name: &str, cost: i64) -> ProductDraft {
        ProductDraft { barcode: barcode.into(), name: name.into(), cost_cents: cost, group_id: None, gct: true }
    }

    #[test]
    fn markup_price() {
        assert_eq!(price_from_markup(10_000, 0.25), 12_500);
        assert_eq!(price_from_markup(10_000, 0.35), 13_500);
        assert_eq!(price_from_markup(333, 0.25), 416); // 416.25 -> 416
        assert_eq!(price_from_markup(10_000, 0.0), 10_000);
        assert_eq!(price_from_markup(0, 0.25), 0);
        assert_eq!(price_from_markup(10_000, f64::NAN), 10_000);
    }

    #[test]
    fn cost_text() {
        for (text, cents) in [("", 0), ("12", 1200), ("12.5", 1250), ("12.50", 1250), (".5", 50), ("1,200.00", 120_000), ("$3.99", 399), (" 7 ", 700), ("999999.00", 99_999_900)] {
            assert_eq!(parse_cost(text).unwrap(), cents, "{text:?}");
        }
        for bad in ["abc", "1.2.3", "-5", "12.345", "1e3", ".", "1000000", "999999.01"] {
            assert!(parse_cost(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn write_is_normalised_and_priced() {
        let w = ProductDraft { group_id: Some(3), ..draft(" 12ab ", " coke 330ml ", 10_000) }.into_write(Some(0.35)).unwrap();
        assert_eq!((w.barcode.as_str(), w.name.as_str(), w.cost_cents, w.price_cents, w.group_id), ("12AB", "COKE 330ML", 10_000, 13_500, Some(3)));
        assert_eq!(draft("1", "x", 500).into_write(None).unwrap().price_cents, 500);
    }

    #[test]
    fn required_fields() {
        let e = |d: ProductDraft| d.into_write(None).unwrap_err().to_string();
        assert_eq!(e(draft("", "X", 0)), "Barcode and Name are required.");
        assert_eq!(e(draft("1", "  ", 0)), "Barcode and Name are required.");
        assert_eq!(e(draft("1", "X", -1)), "Enter a valid cost.");
    }

    #[test]
    fn barcode_change_warns_only_when_it_really_changes() {
        let original = ProductDetail {
            id: 1, barcode: "ABC123".into(), name: "X".into(), group_id: None, cost_cents: 0,
            price_cents: 0, gct: true, membership: Membership::Standalone,
        };
        assert!(warnings_for_edit(&original, &draft(" abc123 ", "X", 0)).is_empty(), "case and spaces only");
        let w = warnings_for_edit(&original, &draft("zzz", "X", 0));
        assert_eq!(w, vec![ProductWarning::BarcodeChange { old: "ABC123".into(), new: "ZZZ".into() }]);
        assert!(w[0].message().contains("ABC123") && w[0].message().contains("ZZZ"));
    }

    #[test]
    fn hint() {
        assert_eq!(price_hint(10_000, 0.25, "$"), "= $100.00 \u{d7} (1 + 25%) = $125.00");
        assert_eq!(price_hint(10_000, 0.0, "$"), "");
        assert_eq!(price_hint(0, 0.25, "$"), "");
    }
}
