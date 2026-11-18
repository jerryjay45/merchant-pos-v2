//! Products as the supervisor's list shows them: what it costs and sells for *now*, after the
//! group rules are applied, plus the paging that goes with a long list.

use std::sync::Arc;

use crate::repo::StorageError;

/// How a product gets its price and stock. A product is at most one of these (the products table
/// has a CHECK for it), and the list shows the group's values for a grouped product, not the
/// product's own columns, which are only authoritative when it is standalone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    Standalone,
    /// Same price as the other products in this alias group (7up, Pepsi, Mtn Dew at $45).
    Alias(String),
    /// Flavours, scents ...: price and stock are shared by the group.
    Variant(String),
    /// A case/box/6-pack linked to its single product.
    Case,
}

/// One line of the product list. Money is in cents, already resolved:
/// variant group, then alias group, then the product's own value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductRow {
    pub id: i64,
    pub barcode: String,
    pub name: String,
    pub group_name: Option<String>,
    pub cost_cents: i64,
    pub price_cents: i64,
    pub stock: i64,
    pub gct: bool,
    pub membership: Membership,
}

impl ProductRow {
    /// The small tags in the list's "Tags" column, like the Python app: `GCT`, `CASE`.
    pub fn tags(&self) -> Vec<&'static str> {
        let mut t = Vec::new();
        if self.gct {
            t.push("GCT");
        }
        if self.membership == Membership::Case {
            t.push("CASE");
        }
        t
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductQuery {
    /// Matches barcode, name, alias-group name and variant-group name as a plain,
    /// case-insensitive substring (`%` and `_` mean themselves). Empty = everything.
    pub search: String,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductPage {
    pub rows: Vec<ProductRow>,
    /// All products matching the search, not just this page.
    pub total: u64,
}

pub trait ProductRepository: Send + Sync {
    fn list(&self, query: &ProductQuery) -> Result<ProductPage, StorageError>;
}

impl<T: ProductRepository + ?Sized> ProductRepository for Arc<T> {
    fn list(&self, q: &ProductQuery) -> Result<ProductPage, StorageError> {
        (**self).list(q)
    }
}

/// Page arithmetic for a list that is loaded a page at a time. Pages are 0-based inside,
/// 1-based in the text shown to people.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pager {
    pub page: u32,
    pub page_size: u32,
    pub total: u64,
}

impl Pager {
    pub fn new(page_size: u32) -> Self {
        Self { page: 0, page_size: page_size.max(1), total: 0 }
    }

    pub fn page_count(&self) -> u32 {
        (((self.total + self.page_size as u64 - 1) / self.page_size as u64).max(1)).min(u32::MAX as u64) as u32
    }

    pub fn offset(&self) -> u32 {
        self.page.saturating_mul(self.page_size)
    }

    pub fn has_prev(&self) -> bool {
        self.page > 0
    }

    pub fn has_next(&self) -> bool {
        self.page + 1 < self.page_count()
    }

    /// After the list reloads the total can shrink (a delete, a narrower search): never stay on a
    /// page that no longer exists.
    pub fn set_total(&mut self, total: u64) {
        self.total = total;
        self.page = self.page.min(self.page_count() - 1);
    }

    pub fn prev(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    pub fn next(&mut self) {
        if self.has_next() {
            self.page += 1;
        }
    }

    pub fn first(&mut self) {
        self.page = 0;
    }

    /// `Page 3 of 41`
    pub fn page_label(&self) -> String {
        format!("Page {} of {}", self.page + 1, self.page_count())
    }

    /// `101-150 of 2,030 products`, `1 product`, or `No products`.
    pub fn range_label(&self, shown: usize) -> String {
        match (self.total, shown) {
            (0, _) | (_, 0) => "No products".to_string(),
            (1, _) => "1 product".to_string(),
            (total, n) => {
                let first = self.offset() as u64 + 1;
                format!("{}-{} of {} products", group(first), group(first + n as u64 - 1), group(total))
            }
        }
    }
}

fn group(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager(total: u64, page: u32) -> Pager {
        let mut p = Pager::new(50);
        p.total = total;
        p.page = page;
        p
    }

    #[test]
    fn counts_pages() {
        assert_eq!(pager(0, 0).page_count(), 1);
        assert_eq!(pager(1, 0).page_count(), 1);
        assert_eq!(pager(50, 0).page_count(), 1);
        assert_eq!(pager(51, 0).page_count(), 2);
        assert_eq!(pager(2030, 0).page_count(), 41);
    }

    #[test]
    fn prev_next_stop_at_the_ends() {
        let mut p = pager(120, 0); // 3 pages
        assert!(!p.has_prev() && p.has_next());
        p.prev();
        assert_eq!(p.page, 0);
        p.next();
        p.next();
        assert_eq!((p.page, p.has_next()), (2, false));
        p.next();
        assert_eq!(p.page, 2);
        assert_eq!(p.offset(), 100);
    }

    #[test]
    fn shrinking_total_pulls_the_page_back() {
        let mut p = pager(500, 9);
        p.set_total(120);
        assert_eq!(p.page, 2);
        p.set_total(0);
        assert_eq!(p.page, 0);
    }

    #[test]
    fn labels() {
        assert_eq!(pager(2030, 2).page_label(), "Page 3 of 41");
        assert_eq!(pager(2030, 2).range_label(50), "101-150 of 2,030 products");
        assert_eq!(pager(2030, 40).range_label(30), "2,001-2,030 of 2,030 products");
        assert_eq!(pager(1, 0).range_label(1), "1 product");
        assert_eq!(pager(0, 0).range_label(0), "No products");
    }

    #[test]
    fn tags_match_the_python_list() {
        let mut r = ProductRow {
            id: 1, barcode: "1".into(), name: "X".into(), group_name: None, cost_cents: 0,
            price_cents: 0, stock: 0, gct: true, membership: Membership::Standalone,
        };
        assert_eq!(r.tags(), ["GCT"]);
        r.membership = Membership::Case;
        assert_eq!(r.tags(), ["GCT", "CASE"]);
        r.gct = false;
        assert_eq!(r.tags(), ["CASE"]);
        r.membership = Membership::Variant("Flavours".into());
        assert!(r.tags().is_empty());
    }
}
