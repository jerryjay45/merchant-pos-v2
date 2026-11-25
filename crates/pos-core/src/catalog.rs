//! The shop-wide catalogue settings behind the Groups & Discounts page: markup groups (MEATS 35 %,
//! GENERAL 25 %), the shared discount levels, and the case profit. Pure rules and the storage
//! interface; no UI and no database driver.
//!
//! Note on words: the database column is called `profit_margin`, but it has always been a
//! *markup on cost* (price = cost x (1 + markup)); the page calls it Markup so nobody mixes it up
//! with a margin on price.

use crate::{
    discount::{fmt_percent, DiscountLevel, MAX_MIN_QTY, MAX_PERCENT, MIN_PERCENT},
    product_edit::ProductError,
    repo::StorageError,
};

/// Highest markup the page accepts, in percent.
pub const MAX_MARKUP_PERCENT: f64 = 1000.0;
pub const MAX_NAME_LEN: usize = 40;

/// A markup group with how many products use it.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkupGroupRow {
    pub id: i64,
    pub name: String,
    /// A fraction: 0.35 = 35 %.
    pub markup: f64,
    pub products: u32,
}

/// A shared discount level with how many products and groups use it.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelRow {
    pub level: DiscountLevel,
    pub in_use: u32,
}

/// A level as typed on the page, already checked.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelInput {
    pub name: String,
    pub min_qty: u32,
    /// In percent: 5.0 = 5 %.
    pub percent: f64,
}

/// A new markup for one group.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkupChange {
    pub group_id: i64,
    /// A fraction: 0.35 = 35 %.
    pub markup: f64,
}

/// What saving markups did to prices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarkupOutcome {
    /// Standalone products in the changed groups whose selling price changed.
    pub products_repriced: u32,
    /// Case products that followed a repriced single.
    pub cases_repriced: Vec<String>,
}

/// "35", "35%", "12,5" -> 0.35 / 0.125. Always a percentage, never a fraction (the Python page
/// guessed, so typing 1 meant 100 %).
pub fn parse_markup_percent(text: &str) -> Result<f64, String> {
    let t = text.trim().trim_end_matches('%').trim().replace(',', ".");
    let v: f64 = t.parse().map_err(|_| "Enter the markup as a number, like 25 or 12.5.".to_string())?;
    if !v.is_finite() || !(0.0..=MAX_MARKUP_PERCENT).contains(&v) {
        return Err(format!("The markup must be between 0 and {MAX_MARKUP_PERCENT} %."));
    }
    Ok((v * 100.0).round() / 10_000.0) // to 0.01 %
}

/// A fraction back to text for the box: 0.35 -> "35".
pub fn markup_text(markup: f64) -> String {
    fmt_percent((markup * 10_000.0).round() / 100.0)
}

/// A level typed on the page.
pub fn parse_level(name: &str, qty: &str, percent: &str) -> Result<LevelInput, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Every discount level needs a name.".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("Level names can be at most {MAX_NAME_LEN} characters."));
    }
    let min_qty: u32 = qty.trim().parse().map_err(|_| format!("{name}: enter the minimum quantity as a whole number."))?;
    if !(1..=MAX_MIN_QTY).contains(&min_qty) {
        return Err(format!("{name}: the minimum quantity must be between 1 and {MAX_MIN_QTY}."));
    }
    let percent: f64 = percent
        .trim()
        .trim_end_matches('%')
        .replace(',', ".")
        .parse()
        .map_err(|_| format!("{name}: enter the discount as a number, like 5 or 12.5."))?;
    if !percent.is_finite() || !(MIN_PERCENT..=MAX_PERCENT).contains(&percent) {
        return Err(format!("{name}: the discount must be between {MIN_PERCENT} and {MAX_PERCENT} %."));
    }
    Ok(LevelInput { name, min_qty, percent: (percent * 10.0).round() / 10.0 })
}

/// Group names are stored upper-case, like the Python app.
pub fn clean_group_name(name: &str) -> Result<String, ProductError> {
    let n = name.trim().to_uppercase();
    if n.is_empty() {
        return Err(ProductError::Invalid("Enter a group name.".into()));
    }
    if n.chars().count() > MAX_NAME_LEN {
        return Err(ProductError::Invalid(format!("Group names can be at most {MAX_NAME_LEN} characters.")));
    }
    Ok(n)
}

/// One page of a list, found in the database (not by loading everything and slicing).
#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    /// Part of the name, any case; empty = everything.
    pub search: String,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListPage<T> {
    pub rows: Vec<T>,
    /// All rows that match the search, not just this page.
    pub total: u64,
}

pub trait CatalogRepository: Send + Sync {
    /// Markup groups by name, one page at a time.
    fn markup_groups(&self, query: &ListQuery) -> Result<ListPage<MarkupGroupRow>, StorageError>;
    /// Adds a group with a 0 % markup. The name is upper-cased; a name that exists is refused.
    fn add_markup_group(&self, name: &str) -> Result<MarkupGroupRow, ProductError>;
    /// Products in the group keep their prices and just lose the group.
    fn delete_markup_group(&self, id: i64) -> Result<(), StorageError>;
    /// How many standalone products would get a different selling price from these markups.
    fn count_repriced_by_markups(&self, changes: &[MarkupChange]) -> Result<u32, StorageError>;
    /// Sets the markups and reprices the standalone products of those groups (cost x (1 + markup)),
    /// then the cases that follow a repriced single, in one transaction.
    fn save_markups(&self, changes: &[MarkupChange], case_profit_pct: f64) -> Result<MarkupOutcome, StorageError>;

    /// Discount levels, smallest minimum quantity first, one page at a time.
    fn level_rows(&self, query: &ListQuery) -> Result<ListPage<LevelRow>, StorageError>;
    fn add_level(&self, input: &LevelInput) -> Result<LevelRow, ProductError>;
    fn update_level(&self, id: i64, input: &LevelInput) -> Result<(), ProductError>;
    /// Products and groups that used the level lose that discount.
    fn delete_level(&self, id: i64) -> Result<(), StorageError>;

    /// Reprices every case from its single or variant group, by each case's own price mode (a
    /// "cost + profit" case at `case_profit_pct`). Returns the names of the cases that changed.
    fn recalc_all_cases(&self, case_profit_pct: f64) -> Result<Vec<String>, StorageError>;
}

impl<T: CatalogRepository + ?Sized> CatalogRepository for std::sync::Arc<T> {
    fn markup_groups(&self, q: &ListQuery) -> Result<ListPage<MarkupGroupRow>, StorageError> {
        (**self).markup_groups(q)
    }
    fn add_markup_group(&self, name: &str) -> Result<MarkupGroupRow, ProductError> {
        (**self).add_markup_group(name)
    }
    fn delete_markup_group(&self, id: i64) -> Result<(), StorageError> {
        (**self).delete_markup_group(id)
    }
    fn count_repriced_by_markups(&self, c: &[MarkupChange]) -> Result<u32, StorageError> {
        (**self).count_repriced_by_markups(c)
    }
    fn save_markups(&self, c: &[MarkupChange], pct: f64) -> Result<MarkupOutcome, StorageError> {
        (**self).save_markups(c, pct)
    }
    fn level_rows(&self, q: &ListQuery) -> Result<ListPage<LevelRow>, StorageError> {
        (**self).level_rows(q)
    }
    fn add_level(&self, i: &LevelInput) -> Result<LevelRow, ProductError> {
        (**self).add_level(i)
    }
    fn update_level(&self, id: i64, i: &LevelInput) -> Result<(), ProductError> {
        (**self).update_level(id, i)
    }
    fn delete_level(&self, id: i64) -> Result<(), StorageError> {
        (**self).delete_level(id)
    }
    fn recalc_all_cases(&self, pct: f64) -> Result<Vec<String>, StorageError> {
        (**self).recalc_all_cases(pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_always_a_percentage() {
        for (text, fraction) in [("25", 0.25), (" 35% ", 0.35), ("12,5", 0.125), ("0", 0.0), ("1", 0.01), ("100", 1.0), ("1000", 10.0), ("7.255", 0.0726)] {
            assert_eq!(parse_markup_percent(text).unwrap(), fraction, "{text:?}");
        }
        for bad in ["", "abc", "-1", "1000.1", "NaN", "inf", "2 5"] {
            assert!(parse_markup_percent(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn markup_text_reads_back() {
        assert_eq!(markup_text(0.35), "35");
        assert_eq!(markup_text(0.125), "12.5");
        assert_eq!(markup_text(0.0), "0");
        assert_eq!(markup_text(0.07), "7");
        assert_eq!(markup_text(0.35000000000000003), "35");
    }

    #[test]
    fn levels() {
        let ok = parse_level(" Level 1 ", "3", "5").unwrap();
        assert_eq!((ok.name.as_str(), ok.min_qty, ok.percent), ("Level 1", 3, 5.0));
        assert_eq!(parse_level("L", "6", "12,5%").unwrap().percent, 12.5);
        let err = |n: &str, q: &str, p: &str| parse_level(n, q, p).unwrap_err();
        assert!(err("", "3", "5").contains("needs a name"));
        assert!(err("L", "0", "5").contains("between 1 and 9999"));
        assert!(err("L", "x", "5").contains("whole number"));
        assert!(err("L", "3", "0").contains("between 0.1 and 99.9"));
        assert!(err("L", "3", "100").contains("between 0.1 and 99.9"));
        assert!(err("L", "3", "five").contains("as a number"));
        assert!(err(&"x".repeat(41), "3", "5").contains("at most 40"));
    }

    #[test]
    fn group_names() {
        assert_eq!(clean_group_name(" meats ").unwrap(), "MEATS");
        assert!(clean_group_name("  ").is_err());
        assert!(clean_group_name(&"x".repeat(41)).is_err());
    }
}
