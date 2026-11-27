//! Stock storage: lists, adjustments and history, on `products.db`.

use pos_core::{
    catalog::{ListPage, ListQuery},
    product_edit::ProductError,
    repo::StorageError,
    stock::{History, HistoryRow, PoolRow, StockChange, StockFilter, StockRepository, StockRow, StockSummary},
};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

use crate::{
    products::{like_literal, search_clause, ProductsDb},
    storage,
};

const PRODUCT_JOINS: &str = "
    FROM products p
    LEFT JOIN groups         g  ON g.id  = p.group_id
    LEFT JOIN alias_groups   ag ON ag.id = p.alias_group_id
    LEFT JOIN variant_groups vg ON vg.id = p.variant_group_id";

fn stock_row(r: &rusqlite::Row) -> rusqlite::Result<StockRow> {
    Ok(StockRow {
        id: r.get(0)?,
        name: r.get(1)?,
        group: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        stock: r.get(3)?,
        shared_with: r.get(4)?,
    })
}

const SELECT_ROW: &str = "SELECT p.id, p.name, g.name, COALESCE(vg.stock, p.stock), vg.name";

/// Reason text kept with the history: trimmed, never empty, short.
fn clean_reason(reason: &str) -> String {
    let r: String = reason.trim().chars().take(40).collect();
    if r.is_empty() { "Other".to_string() } else { r }
}

/// Changes a variant group's pool by `delta` (never below 0) and logs what really moved.
fn adjust_pool_tx(conn: &Connection, group_id: i64, delta: i64, reason: &str, user_id: Option<i64>) -> Result<StockChange, ProductError> {
    let (name, before): (String, i64) = conn
        .query_row("SELECT name, stock FROM variant_groups WHERE id = ?1", [group_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(storage)?
        .ok_or(ProductError::NotFound)?;
    let after = (before + delta).max(0);
    let applied = after - before;
    if applied != 0 {
        conn.execute("UPDATE variant_groups SET stock = ?1 WHERE id = ?2", params![after, group_id]).map_err(storage)?;
        conn.execute(
            "INSERT INTO stock_adjustments (variant_group_id, qty_change, reason, adjusted_by) VALUES (?1, ?2, ?3, ?4)",
            params![group_id, applied, clean_reason(reason), user_id],
        )
        .map_err(storage)?;
    }
    Ok(StockChange { before, after, applied, shared_with: Some(name) })
}

fn history_of(conn: &Connection, column: &str, id: i64, title: String, limit: u32) -> Result<History, StorageError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT qty_change, reason, adjusted_by, adjusted_at FROM stock_adjustments
             WHERE {column} = ?1 ORDER BY adjusted_at DESC, id DESC LIMIT ?2"
        ))
        .map_err(storage)?;
    let rows = stmt
        .query_map(params![id, limit], |r| Ok(HistoryRow { change: r.get(0)?, reason: r.get(1)?, by: r.get(2)?, at: r.get(3)? }))
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    Ok(History { title, rows })
}

impl StockRepository for ProductsDb {
    fn stock_summary(&self, threshold: i64) -> Result<StockSummary, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        // products that carry their own stock; variant-group members are covered by their group's pool
        let (out, low): (i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(stock <= 0), 0), COALESCE(SUM(stock > 0 AND stock <= ?1), 0)
                 FROM products WHERE is_case = 0 AND variant_group_id IS NULL",
                [threshold],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(storage)?;
        let names = |sql: &str| -> Result<Vec<String>, StorageError> {
            let mut stmt = conn.prepare(sql).map_err(storage)?;
            let rows = stmt.query_map([threshold], |r| r.get(0)).map_err(storage)?.collect::<Result<Vec<_>, _>>().map_err(storage)?;
            Ok(rows)
        };
        let examples = names(
            "SELECT name FROM products WHERE is_case = 0 AND variant_group_id IS NULL AND stock <= ?1
             ORDER BY stock, name COLLATE NOCASE LIMIT 4",
        )?;
        let pools_low: i64 = conn
            .query_row("SELECT COUNT(*) FROM variant_groups WHERE stock <= ?1", [threshold], |r| r.get(0))
            .map_err(storage)?;
        let pool_examples = names("SELECT name FROM variant_groups WHERE stock <= ?1 ORDER BY stock, name COLLATE NOCASE LIMIT 2")?;
        Ok(StockSummary { out: out as u64, low: low as u64, examples, pools_low: pools_low as u64, pool_examples })
    }

    fn stock_list(&self, filter: StockFilter, threshold: i64, q: &ListQuery) -> Result<ListPage<StockRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        // All: every product that isn't a case (a variant member shows its group's stock).
        // Low / Out: products with their own stock; their groups' pools have their own list.
        let (cond, order, mut params): (&str, &str, Vec<Value>) = match filter {
            StockFilter::All => ("p.is_case = 0", "p.name COLLATE NOCASE, p.id", Vec::new()),
            StockFilter::Low => (
                "p.is_case = 0 AND p.variant_group_id IS NULL AND p.stock > 0 AND p.stock <= ?",
                "p.stock, p.name COLLATE NOCASE, p.id",
                vec![Value::Integer(threshold)],
            ),
            StockFilter::Out => ("p.is_case = 0 AND p.variant_group_id IS NULL AND p.stock <= 0", "p.name COLLATE NOCASE, p.id", Vec::new()),
        };
        let mut where_sql = format!(" WHERE {cond}");
        if !q.search.trim().is_empty() {
            let (frag, p) = search_clause(&conn, q.search.trim());
            where_sql.push_str(&frag);
            params.extend(p);
        }
        let total: i64 = conn
            .query_row(&format!("SELECT COUNT(*){PRODUCT_JOINS}{where_sql}"), params_from_iter(params.iter()), |r| r.get(0))
            .map_err(storage)?;
        let mut page_params = params;
        page_params.push(Value::Integer(q.limit as i64));
        page_params.push(Value::Integer(q.offset as i64));
        let mut stmt = conn
            .prepare(&format!("{SELECT_ROW}{PRODUCT_JOINS}{where_sql} ORDER BY {order} LIMIT ? OFFSET ?"))
            .map_err(storage)?;
        let rows = stmt
            .query_map(params_from_iter(page_params.iter()), stock_row)
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(ListPage { rows, total: total.max(0) as u64 })
    }

    fn pool_list(&self, q: &ListQuery) -> Result<ListPage<PoolRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let like = like_literal(q.search.trim());
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM variant_groups WHERE name LIKE ?1 ESCAPE '\\'", [&like], |r| r.get(0))
            .map_err(storage)?;
        let mut stmt = conn
            .prepare("SELECT id, name, stock FROM variant_groups WHERE name LIKE ?1 ESCAPE '\\' ORDER BY name COLLATE NOCASE LIMIT ?2 OFFSET ?3")
            .map_err(storage)?;
        let rows = stmt
            .query_map(params![like, q.limit, q.offset], |r| Ok(PoolRow { id: r.get(0)?, name: r.get(1)?, stock: r.get(2)? }))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(ListPage { rows, total: total.max(0) as u64 })
    }

    fn stock_of(&self, product_id: i64) -> Result<Option<StockRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row(&format!("{SELECT_ROW}{PRODUCT_JOINS} WHERE p.id = ?1"), [product_id], stock_row).optional().map_err(storage)
    }

    fn pool_of(&self, group_id: i64) -> Result<Option<PoolRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row("SELECT id, name, stock FROM variant_groups WHERE id = ?1", [group_id], |r| {
            Ok(PoolRow { id: r.get(0)?, name: r.get(1)?, stock: r.get(2)? })
        })
        .optional()
        .map_err(storage)
    }

    fn adjust_product(&self, product_id: i64, delta: i64, reason: &str, user_id: Option<i64>) -> Result<StockChange, ProductError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        let (group, before): (Option<i64>, i64) = tx
            .query_row("SELECT variant_group_id, stock FROM products WHERE id = ?1", [product_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(storage)?
            .ok_or(ProductError::NotFound)?;
        let change = match group {
            // flavours and scents share one pool: it is the group's stock that moves
            Some(g) => adjust_pool_tx(&tx, g, delta, reason, user_id)?,
            None => {
                let after = (before + delta).max(0);
                let applied = after - before;
                if applied != 0 {
                    tx.execute(
                        "UPDATE products SET stock = ?1, updated_at = datetime('now', 'localtime') WHERE id = ?2",
                        params![after, product_id],
                    )
                    .map_err(storage)?;
                    tx.execute(
                        "INSERT INTO stock_adjustments (product_id, qty_change, reason, adjusted_by) VALUES (?1, ?2, ?3, ?4)",
                        params![product_id, applied, clean_reason(reason), user_id],
                    )
                    .map_err(storage)?;
                }
                StockChange { before, after, applied, shared_with: None }
            }
        };
        tx.commit().map_err(storage)?;
        Ok(change)
    }

    fn adjust_pool(&self, group_id: i64, delta: i64, reason: &str, user_id: Option<i64>) -> Result<StockChange, ProductError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        let change = adjust_pool_tx(&tx, group_id, delta, reason, user_id)?;
        tx.commit().map_err(storage)?;
        Ok(change)
    }

    fn product_history(&self, product_id: i64, limit: u32) -> Result<History, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let row: Option<(String, Option<i64>, Option<String>)> = conn
            .query_row(
                "SELECT p.name, p.variant_group_id, vg.name FROM products p LEFT JOIN variant_groups vg ON vg.id = p.variant_group_id WHERE p.id = ?1",
                [product_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(storage)?;
        match row {
            None => Ok(History { title: String::new(), rows: Vec::new() }),
            // a member's stock is the group's, so the group's history is the one that explains it
            Some((name, Some(g), Some(group))) => history_of(&conn, "variant_group_id", g, format!("{name} · shared with {group}"), limit),
            Some((name, _, _)) => history_of(&conn, "product_id", product_id, name, limit),
        }
    }

    fn pool_history(&self, group_id: i64, limit: u32) -> Result<History, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let name: Option<String> = conn.query_row("SELECT name FROM variant_groups WHERE id = ?1", [group_id], |r| r.get(0)).optional().map_err(storage)?;
        history_of(&conn, "variant_group_id", group_id, name.unwrap_or_default(), limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pos_core::{product::ProductRepository, product_edit::{GroupKind, ProductWrite}};

    fn write(barcode: &str, name: &str) -> ProductWrite {
        ProductWrite {
            barcode: barcode.into(), name: name.into(), cost_cents: 100, price_cents: 150, group_id: None,
            alias_group_id: None, variant_group_id: None, gct: true, discounts: Default::default(), case: None,
        }
    }

    fn page(search: &str, limit: u32, offset: u32) -> ListQuery {
        ListQuery { search: search.into(), limit, offset }
    }

    fn names(p: &ListPage<StockRow>) -> Vec<&str> {
        p.rows.iter().map(|r| r.name.as_str()).collect()
    }

    /// COLA 3, WATER 0, MILK 12, BREAD 40, and CHIPS BBQ / CHIPS SALT sharing a pool of 4.
    fn shop() -> (ProductsDb, i64, i64) {
        let db = ProductsDb::open_in_memory().unwrap();
        for (b, n, s) in [("1", "COLA", 3), ("2", "WATER", 0), ("3", "MILK", 12), ("4", "BREAD", 40)] {
            let id = db.add(&write(b, n), 0.10).unwrap();
            db.with_conn(|c| c.execute("UPDATE products SET stock = ?1 WHERE id = ?2", [s, id])).unwrap();
        }
        let g = db.create_price_group(GroupKind::Variant, "CHIPS").unwrap().id;
        let mut a = write("5", "CHIPS BBQ");
        a.variant_group_id = Some(g);
        let bbq = db.add(&a, 0.10).unwrap();
        let mut b = write("6", "CHIPS SALT");
        b.variant_group_id = Some(g);
        db.add(&b, 0.10).unwrap();
        db.with_conn(|c| c.execute("UPDATE variant_groups SET stock = 4 WHERE id = ?1", [g])).unwrap();
        (db, bbq, g)
    }

    #[test]
    fn the_three_lists_and_the_summary_follow_the_threshold() {
        let (db, _, _) = shop();
        let all = db.stock_list(StockFilter::All, 5, &page("", 50, 0)).unwrap();
        assert_eq!(names(&all), ["BREAD", "CHIPS BBQ", "CHIPS SALT", "COLA", "MILK", "WATER"]);
        assert_eq!(all.rows[1].stock, 4, "a variant member shows the shared pool");
        assert_eq!(all.rows[1].shared_with.as_deref(), Some("CHIPS"));
        let low = db.stock_list(StockFilter::Low, 5, &page("", 50, 0)).unwrap();
        assert_eq!(names(&low), ["COLA"], "1 to 5; WATER is out, the chips are covered by their pool");
        assert_eq!(names(&db.stock_list(StockFilter::Out, 5, &page("", 50, 0)).unwrap()), ["WATER"]);
        assert_eq!(names(&db.stock_list(StockFilter::Low, 12, &page("", 50, 0)).unwrap()), ["COLA", "MILK"], "emptiest first");
        let s = db.stock_summary(5).unwrap();
        assert_eq!((s.out, s.low, s.pools_low), (1, 1, 1));
        assert_eq!(s.examples, ["WATER", "COLA"]);
        assert_eq!(s.pool_examples, ["CHIPS"]);
        assert!(s.alert_text(5).unwrap().contains("Variant groups low: CHIPS"));
    }

    #[test]
    fn lists_page_and_search() {
        let (db, _, _) = shop();
        let p = db.stock_list(StockFilter::All, 5, &page("", 2, 2)).unwrap();
        assert_eq!((names(&p), p.total), (vec!["CHIPS SALT", "COLA"], 6));
        assert_eq!(names(&db.stock_list(StockFilter::All, 5, &page("chips", 50, 0)).unwrap()), ["CHIPS BBQ", "CHIPS SALT"]);
        assert_eq!(names(&db.stock_list(StockFilter::All, 5, &page("4", 50, 0)).unwrap()), ["BREAD"], "search matches barcodes too");
        assert_eq!(db.stock_list(StockFilter::Out, 5, &page("cola", 50, 0)).unwrap().total, 0);
        let pools = db.pool_list(&page("chi", 20, 0)).unwrap();
        assert_eq!((pools.total, pools.rows[0].stock), (1, 4));
        assert_eq!(db.pool_of(pools.rows[0].id).unwrap().unwrap().name, "CHIPS");
        assert_eq!(db.pool_of(9999).unwrap(), None);
    }

    #[test]
    fn adding_and_removing_stock_is_logged_and_never_goes_below_zero() {
        let (db, _, _) = shop();
        let cola = db.stock_list(StockFilter::All, 5, &page("cola", 5, 0)).unwrap().rows[0].id;
        let c = db.adjust_product(cola, 12, "Restock", Some(7)).unwrap();
        assert_eq!((c.before, c.after, c.applied), (3, 15, 12));
        // asking for more than is there removes what is there, and the log says so
        let c = db.adjust_product(cola, -40, "Damaged / Lost", Some(7)).unwrap();
        assert_eq!((c.before, c.after, c.applied), (15, 0, -15));
        assert_eq!(c.message(-40), "Only 15 were in stock: removed 15. Stock is now 0.");
        // removing from zero changes and logs nothing
        let c = db.adjust_product(cola, -1, "Correction", Some(7)).unwrap();
        assert_eq!(c.applied, 0);
        let h = db.product_history(cola, 200).unwrap();
        assert_eq!(h.title, "COLA");
        assert_eq!(h.rows.iter().map(|r| (r.change, r.reason.as_str(), r.by)).collect::<Vec<_>>(), [(-15, "Damaged / Lost", Some(7)), (12, "Restock", Some(7))]);
        assert_eq!(db.stock_of(cola).unwrap().unwrap().stock, 0);
        assert_eq!(db.adjust_product(9999, 1, "Restock", None), Err(ProductError::NotFound));
    }

    #[test]
    fn variant_members_move_the_shared_pool_and_share_its_history() {
        let (db, bbq, g) = shop();
        let salt = db.stock_list(StockFilter::All, 5, &page("salt", 5, 0)).unwrap().rows[0].id;
        let c = db.adjust_product(bbq, 20, "Restock", Some(1)).unwrap();
        assert_eq!((c.before, c.after, c.shared_with.as_deref()), (4, 24, Some("CHIPS")));
        assert_eq!(db.stock_of(salt).unwrap().unwrap().stock, 24, "the other flavour sees the same pool");
        db.adjust_product(salt, -4, "Correction", Some(2)).unwrap();
        assert_eq!(db.pool_list(&page("", 10, 0)).unwrap().rows[0].stock, 20);
        // either flavour's history is the pool's history
        let hb = db.product_history(bbq, 10).unwrap();
        let hs = db.product_history(salt, 10).unwrap();
        assert_eq!(hb.rows, hs.rows);
        assert_eq!(hb.title, "CHIPS BBQ · shared with CHIPS");
        assert_eq!(hb.rows.len(), 2);
        assert_eq!(db.pool_history(g, 10).unwrap().rows, hb.rows);
        // the pool itself can be adjusted directly
        let c = db.adjust_pool(g, -100, "Damaged / Lost", None).unwrap();
        assert_eq!((c.after, c.applied), (0, -20));
        assert_eq!(db.adjust_pool(9999, 1, "Restock", None), Err(ProductError::NotFound));
    }

    #[test]
    fn empty_reasons_become_other_and_history_is_limited_newest_first() {
        let (db, _, _) = shop();
        let id = db.stock_list(StockFilter::All, 5, &page("milk", 5, 0)).unwrap().rows[0].id;
        for i in 1..=5 {
            db.adjust_product(id, i, if i == 3 { "  " } else { "Restock" }, None).unwrap();
        }
        let h = db.product_history(id, 3).unwrap();
        assert_eq!(h.rows.iter().map(|r| r.change).collect::<Vec<_>>(), [5, 4, 3], "newest first, limited");
        assert_eq!(h.rows[2].reason, "Other");
    }

    #[test]
    fn user_names_for_the_history() {
        use pos_core::{repo::UserRepository, Role};
        let users = crate::UsersDb::open_in_memory().unwrap();
        let a = users.insert_user("JANE DOE", "JANE", "x", Role::Manager).unwrap();
        let b = users.insert_user("SAM", "SAM", "x", Role::Supervisor).unwrap();
        let mut names = users.full_names(&[a, b, 999]).unwrap();
        names.sort();
        assert_eq!(names, [(a, "JANE DOE".to_string()), (b, "SAM".to_string())]);
        assert!(users.full_names(&[]).unwrap().is_empty());
    }

    #[test]
    fn the_stock_lists_stay_fast_with_many_products() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60000)
                 INSERT INTO products (barcode, name, cost, selling_price, stock)
                 SELECT 'B' || i, 'ITEM ' || printf('%05d', i), 1.0, 1.25, i % 50 FROM n;",
            )
        })
        .unwrap();
        let started = std::time::Instant::now();
        let summary = db.stock_summary(5).unwrap();
        let low = db.stock_list(StockFilter::Low, 5, &page("", 50, 0)).unwrap();
        let all = db.stock_list(StockFilter::All, 5, &page("", 50, 100)).unwrap();
        let found = db.stock_list(StockFilter::All, 5, &page("item 0299", 50, 0)).unwrap();
        let elapsed = started.elapsed();
        assert_eq!((summary.out, low.total, all.total), (1200, 5 * 1200, 60_000));
        assert_eq!(found.rows.len(), 10);
        eprintln!("summary + three list pages over 60,000 products: {elapsed:?}");
        assert!(elapsed.as_millis() < 1000, "took {elapsed:?}");
    }
}
