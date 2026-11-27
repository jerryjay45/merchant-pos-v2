//! Stock: how much is left, adjusting it, and what to warn about. Pure rules and the storage
//! interface; no UI and no database driver.
//!
//! Stock lives on the product, except for products in a variant group (flavours, scents): they
//! share one pool of stock kept on the group, so selling or adjusting any member moves the pool.

use crate::{catalog::{ListPage, ListQuery}, product_edit::ProductError, repo::StorageError};

/// Largest quantity one adjustment may move (like the Python spin box).
pub const MAX_ADJUST: u32 = 99_999;
/// "Low" means this many or fewer, unless the shop sets `low_stock_threshold`.
pub const DEFAULT_LOW_STOCK: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockLevel {
    Ok,
    Low,
    Out,
}

impl StockLevel {
    pub fn of(stock: i64, threshold: i64) -> Self {
        if stock <= 0 {
            StockLevel::Out
        } else if stock <= threshold {
            StockLevel::Low
        } else {
            StockLevel::Ok
        }
    }

    /// 0 = fine, 1 = low, 2 = out; what the window colours by.
    pub fn code(self) -> i32 {
        match self {
            StockLevel::Ok => 0,
            StockLevel::Low => 1,
            StockLevel::Out => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockFilter {
    All,
    /// 1 up to the low-stock threshold.
    Low,
    /// Nothing left.
    Out,
}

/// Why stock was changed; kept with each adjustment.
pub const REASONS: [&str; 5] = ["Restock", "Damaged / Lost", "Correction", "Return", "Other"];

/// Text typed in the quantity box -> 1 to 99,999.
pub fn parse_qty(text: &str) -> Result<u32, String> {
    let n: u32 = text.trim().parse().map_err(|_| "Enter the quantity as a whole number, like 12.".to_string())?;
    if !(1..=MAX_ADJUST).contains(&n) {
        return Err(format!("The quantity must be between 1 and {MAX_ADJUST}."));
    }
    Ok(n)
}

/// A product in the stock list. `stock` is the effective stock: a variant-group member shows the
/// group's shared stock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockRow {
    pub id: i64,
    pub name: String,
    /// Markup group name ("" = none).
    pub group: String,
    pub stock: i64,
    /// The variant group whose stock this product shares.
    pub shared_with: Option<String>,
}

/// A variant group and its shared stock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRow {
    pub id: i64,
    pub name: String,
    pub stock: i64,
}

/// What the alert bar above the lists is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StockSummary {
    pub out: u64,
    pub low: u64,
    /// A few of the products that are out or low, emptiest first.
    pub examples: Vec<String>,
    pub pools_low: u64,
    pub pool_examples: Vec<String>,
}

impl StockSummary {
    /// "3 products out of stock, 12 low (5 or fewer). Variant groups low: A, B."; `None` when all is well.
    pub fn alert_text(&self, threshold: i64) -> Option<String> {
        if self.out == 0 && self.low == 0 && self.pools_low == 0 {
            return None;
        }
        let mut parts = Vec::new();
        let n = |c: u64, one: &str, many: &str| format!("{c} {}", if c == 1 { one } else { many });
        if self.out > 0 {
            parts.push(format!("{} out of stock", n(self.out, "product", "products")));
        }
        if self.low > 0 {
            parts.push(format!("{} low ({threshold} or fewer)", self.low));
        }
        let mut text = parts.join(", ");
        if !self.examples.is_empty() {
            let more = (self.out + self.low).saturating_sub(self.examples.len() as u64);
            text.push_str(&format!(": {}", self.examples.join(", ")));
            if more > 0 {
                text.push_str(&format!(" and {more} more"));
            }
        }
        if self.pools_low > 0 {
            let sep = if text.is_empty() { "" } else { ". " };
            text.push_str(&format!("{sep}Variant groups low: {}", self.pool_examples.join(", ")));
            let more = self.pools_low.saturating_sub(self.pool_examples.len() as u64);
            if more > 0 {
                text.push_str(&format!(" and {more} more"));
            }
        }
        Some(text)
    }
}

/// What an adjustment did. Stock never goes below zero, so a removal can be smaller than asked;
/// `applied` is what really moved (and what the history records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockChange {
    pub before: i64,
    pub after: i64,
    pub applied: i64,
    /// The variant group, when the product shares its stock.
    pub shared_with: Option<String>,
}

impl StockChange {
    /// One line for the dialog, e.g. "Added 12. Stock is now 40." or "Only 3 were in stock: removed 3."
    pub fn message(&self, asked: i64) -> String {
        let shared = self.shared_with.as_ref().map(|g| format!(" (shared with {g})")).unwrap_or_default();
        if self.applied == 0 {
            return if asked < 0 { format!("Nothing to remove: stock is already 0{shared}.") } else { "Nothing changed.".into() };
        }
        if asked < 0 && self.applied.abs() < asked.abs() {
            return format!("Only {} {} in stock: removed {}. Stock is now 0{shared}.", self.before, if self.before == 1 { "was" } else { "were" }, self.applied.abs());
        }
        let verb = if self.applied > 0 { "Added" } else { "Removed" };
        format!("{verb} {}. Stock is now {}{shared}.", self.applied.abs(), self.after)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub change: i64,
    pub reason: String,
    /// The user id that made the change, if known.
    pub by: Option<i64>,
    /// `YYYY-MM-DD HH:MM:SS`, local time.
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub title: String,
    pub rows: Vec<HistoryRow>,
}

pub trait StockRepository: Send + Sync {
    fn stock_summary(&self, threshold: i64) -> Result<StockSummary, StorageError>;
    /// Products (not cases) for the filter, one page at a time; `search` matches name and barcode,
    /// and the names of a product's alias and variant groups.
    fn stock_list(&self, filter: StockFilter, threshold: i64, q: &ListQuery) -> Result<ListPage<StockRow>, StorageError>;
    /// Variant groups with their shared stock, by name.
    fn pool_list(&self, q: &ListQuery) -> Result<ListPage<PoolRow>, StorageError>;
    fn stock_of(&self, product_id: i64) -> Result<Option<StockRow>, StorageError>;
    fn pool_of(&self, group_id: i64) -> Result<Option<PoolRow>, StorageError>;
    /// Adds (`delta` > 0) or removes (`delta` < 0) stock of a product, or of its variant group's pool
    /// when it is a member. Never goes below 0; the history records what really moved.
    fn adjust_product(&self, product_id: i64, delta: i64, reason: &str, user_id: Option<i64>) -> Result<StockChange, ProductError>;
    fn adjust_pool(&self, group_id: i64, delta: i64, reason: &str, user_id: Option<i64>) -> Result<StockChange, ProductError>;
    /// Newest first. For a variant-group member this is the group's history.
    fn product_history(&self, product_id: i64, limit: u32) -> Result<History, StorageError>;
    fn pool_history(&self, group_id: i64, limit: u32) -> Result<History, StorageError>;
}

impl<T: StockRepository + ?Sized> StockRepository for std::sync::Arc<T> {
    fn stock_summary(&self, t: i64) -> Result<StockSummary, StorageError> {
        (**self).stock_summary(t)
    }
    fn stock_list(&self, f: StockFilter, t: i64, q: &ListQuery) -> Result<ListPage<StockRow>, StorageError> {
        (**self).stock_list(f, t, q)
    }
    fn pool_list(&self, q: &ListQuery) -> Result<ListPage<PoolRow>, StorageError> {
        (**self).pool_list(q)
    }
    fn stock_of(&self, id: i64) -> Result<Option<StockRow>, StorageError> {
        (**self).stock_of(id)
    }
    fn pool_of(&self, id: i64) -> Result<Option<PoolRow>, StorageError> {
        (**self).pool_of(id)
    }
    fn adjust_product(&self, id: i64, d: i64, r: &str, u: Option<i64>) -> Result<StockChange, ProductError> {
        (**self).adjust_product(id, d, r, u)
    }
    fn adjust_pool(&self, id: i64, d: i64, r: &str, u: Option<i64>) -> Result<StockChange, ProductError> {
        (**self).adjust_pool(id, d, r, u)
    }
    fn product_history(&self, id: i64, l: u32) -> Result<History, StorageError> {
        (**self).product_history(id, l)
    }
    fn pool_history(&self, id: i64, l: u32) -> Result<History, StorageError> {
        (**self).pool_history(id, l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(StockLevel::of(0, 5), StockLevel::Out);
        assert_eq!(StockLevel::of(-3, 5), StockLevel::Out);
        assert_eq!(StockLevel::of(1, 5), StockLevel::Low);
        assert_eq!(StockLevel::of(5, 5), StockLevel::Low);
        assert_eq!(StockLevel::of(6, 5), StockLevel::Ok);
        assert_eq!([StockLevel::Ok.code(), StockLevel::Low.code(), StockLevel::Out.code()], [0, 1, 2]);
    }

    #[test]
    fn quantity_text() {
        assert_eq!(parse_qty(" 12 "), Ok(12));
        assert_eq!(parse_qty("99999"), Ok(99_999));
        for bad in ["", "0", "-4", "1.5", "abc", "100000"] {
            assert!(parse_qty(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn change_messages() {
        let c = |before, after, applied| StockChange { before, after, applied, shared_with: None };
        assert_eq!(c(28, 40, 12).message(12), "Added 12. Stock is now 40.");
        assert_eq!(c(40, 30, -10).message(-10), "Removed 10. Stock is now 30.");
        assert_eq!(c(3, 0, -3).message(-10), "Only 3 were in stock: removed 3. Stock is now 0.");
        assert_eq!(c(1, 0, -1).message(-4), "Only 1 was in stock: removed 1. Stock is now 0.");
        assert_eq!(c(0, 0, 0).message(-4), "Nothing to remove: stock is already 0.");
        let shared = StockChange { before: 10, after: 22, applied: 12, shared_with: Some("CHIPS".into()) };
        assert_eq!(shared.message(12), "Added 12. Stock is now 22 (shared with CHIPS).");
    }

    #[test]
    fn alert_text() {
        assert_eq!(StockSummary::default().alert_text(5), None);
        let s = StockSummary { out: 3, low: 12, examples: vec!["A".into(), "B".into(), "C".into(), "D".into()], pools_low: 0, pool_examples: vec![] };
        assert_eq!(s.alert_text(5).unwrap(), "3 products out of stock, 12 low (5 or fewer): A, B, C, D and 11 more");
        let one = StockSummary { out: 1, low: 0, examples: vec!["MILK".into()], pools_low: 2, pool_examples: vec!["CHIPS".into(), "SODA".into()] };
        assert_eq!(one.alert_text(5).unwrap(), "1 product out of stock: MILK. Variant groups low: CHIPS, SODA");
        let pools_only = StockSummary { pools_low: 3, pool_examples: vec!["X".into(), "Y".into()], ..Default::default() };
        assert_eq!(pools_only.alert_text(5).unwrap(), "Variant groups low: X, Y and 1 more");
    }
}
