//! `products.db`: the product list. Same file, tables and search behaviour as the Python app
//! (`core/db_products.py`), so either app can open the other's data folder.

use std::{path::Path, sync::Mutex};

use pos_core::{
    money::to_cents,
    product::{Membership, ProductPage, ProductQuery, ProductRepository, ProductRow},
    repo::StorageError,
};
use rusqlite::{params_from_iter, types::Value, Connection};

use crate::{configure, storage};

const SCHEMA: &str = include_str!("products_schema.sql");
const FTS: &str = include_str!("products_fts.sql");

/// What the list shows: group values win over the product's own columns
/// (variant group, then alias group, then the product), exactly like `_PRODUCT_SELECT` in Python.
const LIST_SELECT: &str = "
    SELECT p.id, p.barcode, p.name, g.name,
           COALESCE(vg.cost, ag.cost, p.cost),
           COALESCE(vg.selling_price, ag.selling_price, p.selling_price),
           COALESCE(vg.stock, p.stock),
           p.gct_applicable, p.is_case, ag.name, vg.name
    FROM   products p
    LEFT   JOIN groups         g  ON g.id  = p.group_id
    LEFT   JOIN alias_groups   ag ON ag.id = p.alias_group_id
    LEFT   JOIN variant_groups vg ON vg.id = p.variant_group_id
    WHERE  1=1";

const COUNT_SELECT: &str = "
    SELECT COUNT(*) FROM products p
    LEFT JOIN alias_groups   ag ON ag.id = p.alias_group_id
    LEFT JOIN variant_groups vg ON vg.id = p.variant_group_id
    WHERE 1=1";

pub struct ProductsDb {
    conn: Mutex<Connection>,
}

impl ProductsDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::init(Connection::open(path).map_err(storage)?)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory().map_err(storage)?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        configure(&conn)?;
        conn.execute_batch(SCHEMA).map_err(storage)?;
        setup_fts(&conn);
        Ok(Self { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.conn.lock().unwrap())
    }
}

/// Creates the search index when this SQLite can (needs FTS5 with the trigram tokenizer);
/// otherwise search silently uses LIKE, which gives the same results more slowly.
fn setup_fts(conn: &Connection) {
    let existed = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE name = 'products_fts'", [], |_| Ok(()))
        .is_ok();
    if conn.execute_batch(FTS).is_err() {
        return;
    }
    if !existed {
        // First time on a database that already has rows: index them.
        let _ = conn.execute("INSERT INTO products_fts(products_fts) VALUES ('rebuild')", []);
    }
}

fn fts_available(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'products_fts'", [], |_| Ok(())).is_ok()
}

/// `%text%` with LIKE wildcards in the text escaped, so a typed `%` or `_` matches itself
/// (the FTS path is literal too, and the two paths must agree).
fn like_literal(text: &str) -> String {
    format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

/// SQL starting with ` AND ` plus its parameters: a case-insensitive substring match on barcode,
/// name, alias-group name and variant-group name.
fn search_clause(conn: &Connection, search: &str) -> (String, Vec<Value>) {
    let text = search.to_lowercase();
    let like = like_literal(&text);
    if text.chars().count() >= 3 && fts_available(conn) {
        let phrase = format!("\"{}\"", text.replace('"', "\"\""));
        return (
            "
            AND p.id IN (
                SELECT rowid FROM products_fts WHERE products_fts MATCH ?
                UNION
                SELECT id FROM products WHERE alias_group_id IN
                    (SELECT id FROM alias_groups WHERE LOWER(name) LIKE ? ESCAPE '\\')
                UNION
                SELECT id FROM products WHERE variant_group_id IN
                    (SELECT id FROM variant_groups WHERE LOWER(name) LIKE ? ESCAPE '\\')
            )"
            .to_string(),
            vec![Value::Text(phrase), Value::Text(like.clone()), Value::Text(like)],
        );
    }
    // Short queries (trigram needs 3+ characters) or no FTS: scan with LIKE.
    (
        "
            AND (LOWER(p.barcode) LIKE ? ESCAPE '\\' OR LOWER(p.name) LIKE ? ESCAPE '\\'
                 OR LOWER(ag.name) LIKE ? ESCAPE '\\' OR LOWER(vg.name) LIKE ? ESCAPE '\\')"
            .to_string(),
        (0..4).map(|_| Value::Text(like.clone())).collect(),
    )
}

impl ProductRepository for ProductsDb {
    fn list(&self, q: &ProductQuery) -> Result<ProductPage, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let search = q.search.trim();
        let (frag, mut params) =
            if search.is_empty() { (String::new(), Vec::new()) } else { search_clause(&conn, search) };

        let total: i64 = conn
            .query_row(&format!("{COUNT_SELECT}{frag}"), params_from_iter(params.iter()), |r| r.get(0))
            .map_err(storage)?;

        // Same order as Python (name, ignoring case); the id keeps equal names in a stable order
        // so a product can't appear on two pages or be skipped between them.
        params.push(Value::Integer(q.limit as i64));
        params.push(Value::Integer(q.offset as i64));
        let sql = format!("{LIST_SELECT}{frag} ORDER BY p.name COLLATE NOCASE, p.id LIMIT ? OFFSET ?");
        let mut stmt = conn.prepare(&sql).map_err(storage)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |r| {
                let alias: Option<String> = r.get(9)?;
                let variant: Option<String> = r.get(10)?;
                let membership = match (variant, alias, r.get::<_, i64>(8)? != 0) {
                    (Some(v), _, _) => Membership::Variant(v),
                    (None, Some(a), _) => Membership::Alias(a),
                    (None, None, true) => Membership::Case,
                    _ => Membership::Standalone,
                };
                Ok(ProductRow {
                    id: r.get(0)?,
                    barcode: r.get(1)?,
                    name: r.get(2)?,
                    group_name: r.get(3)?,
                    cost_cents: to_cents(r.get::<_, f64>(4)?),
                    price_cents: to_cents(r.get::<_, f64>(5)?),
                    stock: r.get(6)?,
                    gct: r.get::<_, i64>(7)? != 0,
                    membership,
                })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;

        Ok(ProductPage { rows, total: total.max(0) as u64 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small catalogue exercising every kind of product.
    fn db() -> ProductsDb {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                "
                INSERT INTO groups (id, name) VALUES (1, 'DRINKS'), (2, 'MEAT');
                INSERT INTO alias_groups   (id, name, cost, selling_price) VALUES (1, 'SODA 500ML', 30.0, 45.0);
                INSERT INTO variant_groups (id, name, cost, selling_price, stock) VALUES (1, 'DOVE SOAP', 120.0, 250.0, 40);

                INSERT INTO products (id, barcode, name, group_id, cost, selling_price, stock, alias_group_id, gct_applicable)
                  VALUES (1, '1001', '7UP 500ML',      1, 99, 99, 5, 1, 1),
                         (2, '1002', 'PEPSI 500ML',    1, 98, 98, 6, 1, 1),
                         (3, '1003', 'MTN DEW 500ML',  1, 97, 97, 7, 1, 1);
                INSERT INTO products (id, barcode, name, group_id, cost, selling_price, stock, variant_group_id, gct_applicable)
                  VALUES (4, '2001', 'DOVE LAVENDER',  NULL, 1, 1, 1, 1, 1),
                         (5, '2002', 'DOVE ROSE',      NULL, 1, 1, 1, 1, 0);
                INSERT INTO products (id, barcode, name, group_id, cost, selling_price, stock, gct_applicable)
                  VALUES (6, '3001', 'CHICKEN BREAST', 2, 450.5, 612.25, 12, 0),
                         (7, '3002', '100% JUICE',     1, 100, 150, 3, 1),
                         (8, '3003', '1000 ISLAND',    1, 10, 20, 3, 1),
                         (9, '3004', 'a lower case name', NULL, 5, 8, 2, 1);
                INSERT INTO products (id, barcode, name, group_id, cost, selling_price, stock, is_case, case_qty, case_product_id, gct_applicable)
                  VALUES (10, '4001', 'CHICKEN CASE', 2, 5000, 6500, 1, 1, 12, 6, 1);
                ",
            )
            .unwrap()
        });
        db
    }

    fn list(db: &ProductsDb, search: &str, limit: u32, offset: u32) -> ProductPage {
        db.list(&ProductQuery { search: search.into(), limit, offset }).unwrap()
    }

    fn names(p: &ProductPage) -> Vec<&str> {
        p.rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn fts_index_exists_and_is_used_for_three_or_more_characters() {
        let d = db();
        assert!(d.with_conn(fts_available), "bundled SQLite should have FTS5 + trigram");
        assert_eq!(names(&list(&d, "chick", 50, 0)), ["CHICKEN BREAST", "CHICKEN CASE"]);
    }

    #[test]
    fn everything_in_name_order_ignoring_case() {
        let all = list(&db(), "", 50, 0);
        assert_eq!(all.total, 10);
        assert_eq!(
            names(&all),
            ["100% JUICE", "1000 ISLAND", "7UP 500ML", "a lower case name", "CHICKEN BREAST", "CHICKEN CASE",
             "DOVE LAVENDER", "DOVE ROSE", "MTN DEW 500ML", "PEPSI 500ML"]
        );
    }

    #[test]
    fn variant_group_beats_alias_group_beats_own_values() {
        let all = list(&db(), "", 50, 0);
        let by = |n: &str| all.rows.iter().find(|r| r.name == n).unwrap();
        // alias members show the group's price, not their own 99 / 98 / 97
        assert_eq!((by("7UP 500ML").cost_cents, by("7UP 500ML").price_cents), (3000, 4500));
        assert_eq!(by("PEPSI 500ML").price_cents, 4500);
        // variant members show the group's price AND shared stock
        assert_eq!((by("DOVE ROSE").price_cents, by("DOVE ROSE").stock), (25_000, 40));
        // standalone shows its own, with cents kept exact
        assert_eq!((by("CHICKEN BREAST").cost_cents, by("CHICKEN BREAST").price_cents), (45_050, 61_225));
    }

    #[test]
    fn membership_and_tags() {
        let all = list(&db(), "", 50, 0);
        let by = |n: &str| all.rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by("7UP 500ML").membership, Membership::Alias("SODA 500ML".into()));
        assert_eq!(by("DOVE ROSE").membership, Membership::Variant("DOVE SOAP".into()));
        assert_eq!(by("CHICKEN CASE").membership, Membership::Case);
        assert_eq!(by("CHICKEN BREAST").membership, Membership::Standalone);
        assert_eq!(by("CHICKEN CASE").tags(), ["GCT", "CASE"]);
        assert_eq!(by("DOVE ROSE").tags(), Vec::<&str>::new()); // gct off
        assert_eq!(by("CHICKEN BREAST").group_name.as_deref(), Some("MEAT"));
    }

    #[test]
    fn search_matches_barcode_name_alias_and_variant_group_names() {
        let d = db();
        assert_eq!(names(&list(&d, "pepsi", 50, 0)), ["PEPSI 500ML"]);            // name
        assert_eq!(list(&d, "3001", 50, 0).total, 1);                              // barcode
        assert_eq!(names(&list(&d, "soda", 50, 0)), ["7UP 500ML", "MTN DEW 500ML", "PEPSI 500ML"]); // alias group name
        assert_eq!(names(&list(&d, "dove soap", 50, 0)), ["DOVE LAVENDER", "DOVE ROSE"]);           // variant group name
        assert_eq!(names(&list(&d, "LOWER CASE", 50, 0)), ["a lower case name"]);  // case-insensitive
        assert_eq!(list(&d, "nothing-like-this", 50, 0).total, 0);
    }

    #[test]
    fn short_queries_use_like_and_agree_with_the_index() {
        let d = db();
        // two characters: trigram can't help, so LIKE runs
        assert_eq!(names(&list(&d, "do", 50, 0)), ["DOVE LAVENDER", "DOVE ROSE"]);
        // one character
        assert_eq!(list(&d, "z", 50, 0).total, 0);
        // the same substring found both ways
        assert_eq!(list(&d, "ove", 50, 0).total, list(&d, "ov", 50, 0).total);
    }

    #[test]
    fn percent_and_underscore_match_themselves() {
        let d = db();
        assert_eq!(names(&list(&d, "100%", 50, 0)), ["100% JUICE"]); // '%' is not "anything"
        assert_eq!(list(&d, "%", 50, 0).total, 1);
        assert_eq!(list(&d, "_", 50, 0).total, 0);
        assert_eq!(list(&d, "10_", 50, 0).total, 0);                 // would match "100" if '_' were a wildcard
        assert_eq!(list(&d, "\"", 50, 0).total, 0);                  // quotes are safe
    }

    #[test]
    fn paging_covers_every_product_once() {
        let d = db();
        let all = names(&list(&d, "", 50, 0)).into_iter().map(String::from).collect::<Vec<_>>();
        let mut seen = Vec::new();
        for page in 0..4 {
            let p = list(&d, "", 3, page * 3);
            assert_eq!(p.total, 10, "total is the same on every page");
            seen.extend(p.rows.into_iter().map(|r| r.name));
        }
        assert_eq!(seen, all);
        assert!(list(&d, "", 3, 30).rows.is_empty()); // past the end: empty, not an error
    }

    #[test]
    fn search_with_whitespace_only_is_everything() {
        assert_eq!(list(&db(), "   ", 50, 0).total, 10);
    }

    /// Cross-check against a products.db written by the Python app. Not run by default:
    ///   PRODUCTS_DB_FIXTURE=<products.db> PRODUCTS_QUERIES=<one search per line> PRODUCTS_OUT=<file> \
    ///     cargo test -p pos-db python_fixture -- --ignored
    /// Writes `search<TAB>total<TAB>id,id,...` for pages 1 and 3 so a script can diff it with Python's
    /// `get_products` / `count_products`.
    #[test]
    #[ignore]
    fn python_fixture_dump() {
        let (Ok(db_path), Ok(queries), Ok(out)) = (
            std::env::var("PRODUCTS_DB_FIXTURE"),
            std::env::var("PRODUCTS_QUERIES"),
            std::env::var("PRODUCTS_OUT"),
        ) else {
            return;
        };
        let db = ProductsDb::open(db_path).unwrap();
        let mut text = String::new();
        for q in std::fs::read_to_string(queries).unwrap().lines() {
            for offset in [0u32, 100] {
                let page = list(&db, q, 50, offset);
                let ids: Vec<String> = page.rows.iter().map(|r| format!("{}:{}:{}", r.id, r.cost_cents, r.price_cents)).collect();
                text.push_str(&format!("{q}\t{offset}\t{}\t{}\n", page.total, ids.join(",")));
            }
        }
        std::fs::write(out, text).unwrap();
    }
}
