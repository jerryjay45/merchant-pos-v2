//! `products.db`: the product list. Same file, tables and search behaviour as the Python app
//! (`core/db_products.py`), so either app can open the other's data folder.

use std::{path::Path, sync::Mutex};

use pos_core::{
    discount::{DiscountLevel, DiscountSetting, SlotSetting, SlotSource},
    money::to_cents,
    product::{Membership, ProductPage, ProductQuery, ProductRepository, ProductRow},
    product_edit::{Group, GroupKind, GroupSync, PriceGroup, ProductDetail, ProductError, ProductWrite, UpdateOutcome},
    repo::StorageError,
};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

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
        migrate(&conn)?;
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

/// Columns the Rust app adds to `products` (all optional, so the Python app keeps working on the
/// same file: it names the columns it uses and ignores the rest).
const EXTRA_PRODUCT_COLUMNS: &[(&str, &str)] = &[
    ("discount1_starts", "TEXT DEFAULT NULL"),
    ("discount1_ends", "TEXT DEFAULT NULL"),
    ("discount2_starts", "TEXT DEFAULT NULL"),
    ("discount2_ends", "TEXT DEFAULT NULL"),
    ("discount_stack", "INTEGER NOT NULL DEFAULT 0"),
];

/// Adds the extra columns that are missing. Safe to run on every start.
fn migrate(conn: &Connection) -> Result<(), StorageError> {
    let have: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('products')").map_err(storage)?;
        let rows = stmt.query_map([], |r| r.get(0)).map_err(storage)?.collect::<Result<Vec<_>, _>>().map_err(storage)?;
        rows
    };
    for (name, ddl) in EXTRA_PRODUCT_COLUMNS {
        if !have.iter().any(|h| h == name) {
            conn.execute(&format!("ALTER TABLE products ADD COLUMN {name} {ddl}"), []).map_err(storage)?;
        }
    }
    Ok(())
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

    fn get(&self, id: i64) -> Result<Option<ProductDetail>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        get_detail(&conn, id).map_err(storage)
    }

    fn groups(&self) -> Result<Vec<Group>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn.prepare("SELECT id, name, profit_margin FROM groups ORDER BY name").map_err(storage)?;
        let rows = stmt
            .query_map([], |r| Ok(Group { id: r.get(0)?, name: r.get(1)?, markup: r.get(2)? }))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn discount_levels(&self) -> Result<Vec<DiscountLevel>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare("SELECT id, name, min_qty, percent FROM discount_levels ORDER BY min_qty, id")
            .map_err(storage)?;
        let rows = stmt
            .query_map([], |r| {
                // stored as a fraction (0.05 = 5 %); 4 decimals hides the float noise of x100
                let pct: f64 = r.get(3)?;
                Ok(DiscountLevel {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    min_qty: r.get::<_, i64>(2)?.max(1) as u32,
                    percent: (pct * 100.0 * 1e4).round() / 1e4,
                })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn price_groups(&self, kind: GroupKind) -> Result<Vec<PriceGroup>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare(&format!("SELECT id, name, cost, selling_price FROM {} ORDER BY name", group_table(kind)))
            .map_err(storage)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PriceGroup { id: r.get(0)?, name: r.get(1)?, cost_cents: to_cents(r.get::<_, f64>(2)?), price_cents: to_cents(r.get::<_, f64>(3)?) })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn create_price_group(&self, kind: GroupKind, name: &str) -> Result<PriceGroup, ProductError> {
        let name = name.trim().to_uppercase();
        if name.is_empty() {
            return Err(ProductError::Invalid("Enter a group name.".into()));
        }
        let conn = self.conn.lock().map_err(storage)?;
        let table = group_table(kind);
        let exists = conn
            .query_row(&format!("SELECT 1 FROM {table} WHERE name = ?1"), [&name], |_| Ok(()))
            .optional()
            .map_err(storage)?
            .is_some();
        if exists {
            return Err(ProductError::Invalid(format!("An {} group named \"{name}\" already exists.", kind.label())));
        }
        conn.execute(&format!("INSERT INTO {table} (name) VALUES (?1)"), [&name]).map_err(storage)?;
        Ok(PriceGroup { id: conn.last_insert_rowid(), name, cost_cents: 0, price_cents: 0 })
    }

    fn add(&self, p: &ProductWrite) -> Result<i64, ProductError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        tx.execute(
            "INSERT INTO products (barcode, name, cost, selling_price, group_id, alias_group_id, variant_group_id, gct_applicable,
                                   discount_level1_id, inline_discount1_qty, inline_discount1_pct, discount1_starts, discount1_ends,
                                   discount_level2_id, inline_discount2_qty, inline_discount2_pct, discount2_starts, discount2_ends,
                                   discount_stack)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params_from_iter(
                [
                    Value::Text(p.barcode.clone()),
                    Value::Text(p.name.clone()),
                    Value::Real(dollars(p.cost_cents)),
                    Value::Real(dollars(p.price_cents)),
                    opt_int(p.group_id),
                    opt_int(p.alias_group_id),
                    opt_int(p.variant_group_id),
                    Value::Integer(p.gct as i64),
                ]
                .into_iter()
                .chain(slot_values(&p.discounts.slot1))
                .chain(slot_values(&p.discounts.slot2))
                .chain([Value::Integer(p.discounts.stack as i64)]),
            ),
        )
        .map_err(|e| write_error(e, &p.barcode))?;
        let id = tx.last_insert_rowid();
        // Joining a group that has no price yet: this product sets it.
        if let Some((kind, gid)) = target_group(p) {
            if group_row(&tx, kind, gid).map_err(storage)?.is_some_and(|(_, cost)| cost == 0) {
                push_to_group(&tx, kind, gid, p.cost_cents, p.price_cents).map_err(storage)?;
            }
        }
        tx.commit().map_err(storage)?;
        Ok(id)
    }

    fn update(&self, id: i64, p: &ProductWrite, case_profit_pct: f64) -> Result<UpdateOutcome, ProductError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        let before = get_detail(&tx, id).map_err(storage)?.ok_or(ProductError::NotFound)?;
        if !before.editable() {
            return Err(ProductError::Invalid("Case items can't be edited here yet.".into()));
        }
        tx.execute(
            "UPDATE products SET barcode = ?1, name = ?2, cost = ?3, selling_price = ?4, group_id = ?5,
                    alias_group_id = ?6, variant_group_id = ?7, gct_applicable = ?8,
                    discount_level1_id = ?9, inline_discount1_qty = ?10, inline_discount1_pct = ?11,
                    discount1_starts = ?12, discount1_ends = ?13,
                    discount_level2_id = ?14, inline_discount2_qty = ?15, inline_discount2_pct = ?16,
                    discount2_starts = ?17, discount2_ends = ?18, discount_stack = ?19,
                    updated_at = datetime('now', 'localtime')
             WHERE id = ?20",
            params_from_iter(
                [
                    Value::Text(p.barcode.clone()),
                    Value::Text(p.name.clone()),
                    Value::Real(dollars(p.cost_cents)),
                    Value::Real(dollars(p.price_cents)),
                    opt_int(p.group_id),
                    opt_int(p.alias_group_id),
                    opt_int(p.variant_group_id),
                    Value::Integer(p.gct as i64),
                ]
                .into_iter()
                .chain(slot_values(&p.discounts.slot1))
                .chain(slot_values(&p.discounts.slot2))
                .chain([Value::Integer(p.discounts.stack as i64), Value::Integer(id)]),
            ),
        )
        .map_err(|e| write_error(e, &p.barcode))?;

        let mut out = UpdateOutcome::default();
        let cost_changed = before.cost_cents != p.cost_cents;

        // The group is authoritative: a member's changed cost (or markup group) is pushed up to it.
        // Joining a group instead leaves a priced group alone (the product follows it).
        if let Some((kind, gid)) = target_group(p) {
            if let Some((name, group_cost)) = group_row(&tx, kind, gid).map_err(storage)? {
                let was_member = match kind {
                    GroupKind::Alias => before.alias_group_id == Some(gid),
                    GroupKind::Variant => before.variant_group_id == Some(gid),
                };
                let push = if was_member { cost_changed || before.group_id != p.group_id } else { group_cost == 0 };
                if push {
                    push_to_group(&tx, kind, gid, p.cost_cents, p.price_cents).map_err(storage)?;
                    out.group_sync = Some(GroupSync { group_name: name, cost_cents: p.cost_cents, price_cents: p.price_cents });
                    out.repriced_cases.extend(match kind {
                        GroupKind::Alias => cascade_alias_group(&tx, gid, case_profit_pct),
                        GroupKind::Variant => cascade_variant_group(&tx, gid, case_profit_pct),
                    }?);
                }
            }
        }
        // A changed single-product cost flows to its own cases (cascade_single_cost_to_cases).
        if cost_changed && p.cost_cents > 0 {
            for name in cascade_single(&tx, id, p.cost_cents, case_profit_pct)? {
                if !out.repriced_cases.contains(&name) {
                    out.repriced_cases.push(name);
                }
            }
        }
        tx.commit().map_err(storage)?;
        Ok(out)
    }

    fn delete(&self, id: i64) -> Result<bool, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute("DELETE FROM products WHERE id = ?1", [id]).map(|n| n > 0).map_err(storage)
    }
}

fn opt_int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Integer)
}

fn opt_text(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| Value::Text(s.clone()))
}

/// The five columns of one discount tier, in the order the statements list them:
/// level id, inline min qty, inline percent, first day, last day. A named level and a custom
/// rule exclude each other (a "None" tier clears all five).
fn slot_values(slot: &Option<SlotSetting>) -> [Value; 5] {
    match slot {
        None => [Value::Null, Value::Null, Value::Null, Value::Null, Value::Null],
        Some(s) => {
            let (level, qty, pct) = match &s.source {
                SlotSource::Level(id) => (Value::Integer(*id), Value::Null, Value::Null),
                SlotSource::Custom { min_qty, percent } => (Value::Null, Value::Integer(*min_qty as i64), Value::Real(*percent)),
            };
            [level, qty, pct, opt_text(&s.starts), opt_text(&s.ends)]
        }
    }
}

fn read_slot(level: Option<i64>, qty: Option<i64>, pct: Option<f64>, starts: Option<String>, ends: Option<String>) -> Option<SlotSetting> {
    let source = match (level, qty, pct) {
        (Some(id), _, _) => SlotSource::Level(id),
        (None, Some(q), Some(p)) if q > 0 && p > 0.0 => SlotSource::Custom { min_qty: q as u32, percent: p },
        _ => return None,
    };
    Some(SlotSetting { source, starts, ends })
}

fn dollars(cents: i64) -> f64 {
    cents as f64 / 100.0
}

fn group_table(kind: GroupKind) -> &'static str {
    match kind {
        GroupKind::Alias => "alias_groups",
        GroupKind::Variant => "variant_groups",
    }
}

/// The alias or variant group a write puts the product in (never both; core refuses that).
fn target_group(p: &ProductWrite) -> Option<(GroupKind, i64)> {
    match (p.alias_group_id, p.variant_group_id) {
        (Some(g), _) => Some((GroupKind::Alias, g)),
        (None, Some(g)) => Some((GroupKind::Variant, g)),
        _ => None,
    }
}

/// (name, cost in cents) of a group.
fn group_row(conn: &Connection, kind: GroupKind, id: i64) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(&format!("SELECT name, cost FROM {} WHERE id = ?1", group_table(kind)), [id], |r| {
        Ok((r.get::<_, String>(0)?, to_cents(r.get::<_, f64>(1)?)))
    })
    .optional()
}

fn push_to_group(conn: &Connection, kind: GroupKind, id: i64, cost_cents: i64, price_cents: i64) -> rusqlite::Result<usize> {
    conn.execute(
        &format!("UPDATE {} SET cost = ?1, selling_price = ?2 WHERE id = ?3", group_table(kind)),
        params![dollars(cost_cents), dollars(price_cents), id],
    )
}

/// case cost = unit cost x units (4 decimals), case price = case cost + `pct` (2 decimals).
fn reprice_case(conn: &Connection, case_id: i64, qty: Option<i64>, unit_cost: f64, pct: f64) -> Result<(), StorageError> {
    let case_cost = (unit_cost * qty.unwrap_or(1).max(1) as f64 * 1e4).round() / 1e4;
    let case_price = (case_cost * (1.0 + pct) * 100.0).round() / 100.0;
    conn.execute(
        "UPDATE products SET cost = ?1, selling_price = ?2, updated_at = datetime('now', 'localtime') WHERE id = ?3",
        params![case_cost, case_price, case_id],
    )
    .map_err(storage)?;
    Ok(())
}

fn cases_where(conn: &Connection, filter: &str, arg: i64) -> Result<Vec<(i64, Option<i64>, String)>, StorageError> {
    let mut stmt = conn
        .prepare(&format!("SELECT id, case_qty, name FROM products WHERE is_case = 1 AND {filter}"))
        .map_err(storage)?;
    let rows = stmt
        .query_map([arg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    Ok(rows)
}

fn group_unit_cost(conn: &Connection, kind: GroupKind, gid: i64) -> Result<Option<f64>, StorageError> {
    let cost: Option<f64> = conn
        .query_row(&format!("SELECT cost FROM {} WHERE id = ?1", group_table(kind)), [gid], |r| r.get(0))
        .optional()
        .map_err(storage)?;
    Ok(cost.filter(|c| *c > 0.0))
}

/// Cases of the group's members follow the group's cost (cascade_alias_group_to_cases).
fn cascade_alias_group(conn: &Connection, gid: i64, pct: f64) -> Result<Vec<String>, StorageError> {
    let Some(cost) = group_unit_cost(conn, GroupKind::Alias, gid)? else { return Ok(Vec::new()) };
    let mut names = Vec::new();
    let members: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM products WHERE alias_group_id = ?1").map_err(storage)?;
        let rows = stmt.query_map([gid], |r| r.get(0)).map_err(storage)?.collect::<Result<Vec<_>, _>>().map_err(storage)?;
        rows
    };
    for m in members {
        for (case_id, qty, name) in cases_where(conn, "case_product_id = ?1", m)? {
            reprice_case(conn, case_id, qty, cost, pct)?;
            names.push(name);
        }
    }
    Ok(names)
}

/// Cases linked to the variant group itself follow its cost (cascade_variant_group_to_cases).
fn cascade_variant_group(conn: &Connection, gid: i64, pct: f64) -> Result<Vec<String>, StorageError> {
    let Some(cost) = group_unit_cost(conn, GroupKind::Variant, gid)? else { return Ok(Vec::new()) };
    let mut names = Vec::new();
    for (case_id, qty, name) in cases_where(conn, "case_variant_group_id = ?1", gid)? {
        reprice_case(conn, case_id, qty, cost, pct)?;
        names.push(name);
    }
    Ok(names)
}

/// Cases linked to this single product follow its cost (cascade_single_cost_to_cases).
fn cascade_single(conn: &Connection, product_id: i64, cost_cents: i64, pct: f64) -> Result<Vec<String>, StorageError> {
    let mut names = Vec::new();
    for (case_id, qty, name) in cases_where(conn, "case_product_id = ?1", product_id)? {
        reprice_case(conn, case_id, qty, dollars(cost_cents), pct)?;
        names.push(name);
    }
    Ok(names)
}

/// A UNIQUE failure on the barcode becomes its own error so the form can say so.
fn write_error(e: rusqlite::Error, barcode: &str) -> ProductError {
    if let rusqlite::Error::SqliteFailure(f, msg) = &e {
        let unique_barcode = f.code == rusqlite::ErrorCode::ConstraintViolation
            && msg.as_deref().is_some_and(|m| m.contains("UNIQUE") && m.contains("barcode"));
        if unique_barcode {
            return ProductError::DuplicateBarcode(barcode.to_string());
        }
    }
    ProductError::Storage(storage(e))
}

fn get_detail(conn: &Connection, id: i64) -> rusqlite::Result<Option<ProductDetail>> {
    conn.query_row(
        "SELECT p.id, p.barcode, p.name, p.group_id,
                COALESCE(vg.cost, ag.cost, p.cost), COALESCE(vg.selling_price, ag.selling_price, p.selling_price),
                p.gct_applicable, p.is_case, ag.name, vg.name, p.alias_group_id, p.variant_group_id,
                p.discount_level1_id, p.inline_discount1_qty, p.inline_discount1_pct, p.discount1_starts, p.discount1_ends,
                p.discount_level2_id, p.inline_discount2_qty, p.inline_discount2_pct, p.discount2_starts, p.discount2_ends,
                p.discount_stack
         FROM products p
         LEFT JOIN alias_groups   ag ON ag.id = p.alias_group_id
         LEFT JOIN variant_groups vg ON vg.id = p.variant_group_id
         WHERE p.id = ?1",
        [id],
        |r| {
            let (alias, variant): (Option<String>, Option<String>) = (r.get(8)?, r.get(9)?);
            let membership = match (variant, alias, r.get::<_, i64>(7)? != 0) {
                (Some(v), _, _) => Membership::Variant(v),
                (None, Some(a), _) => Membership::Alias(a),
                (None, None, true) => Membership::Case,
                _ => Membership::Standalone,
            };
            Ok(ProductDetail {
                id: r.get(0)?,
                barcode: r.get(1)?,
                name: r.get(2)?,
                group_id: r.get(3)?,
                alias_group_id: r.get(10)?,
                variant_group_id: r.get(11)?,
                cost_cents: to_cents(r.get::<_, f64>(4)?),
                price_cents: to_cents(r.get::<_, f64>(5)?),
                gct: r.get::<_, i64>(6)? != 0,
                discounts: DiscountSetting {
                    slot1: read_slot(r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?),
                    slot2: read_slot(r.get(17)?, r.get(18)?, r.get(19)?, r.get(20)?, r.get(21)?),
                    stack: r.get::<_, i64>(22)? != 0,
                },
                membership,
            })
        },
    )
    .optional()
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

    // ── add / edit / delete ──

    fn write(barcode: &str, name: &str, cost: i64, price: i64, group: Option<i64>) -> ProductWrite {
        ProductWrite { barcode: barcode.into(), name: name.into(), cost_cents: cost, price_cents: price, group_id: group, alias_group_id: None, variant_group_id: None, gct: true, discounts: Default::default() }
    }

    #[test]
    fn add_then_get_roundtrip_and_appears_in_the_list() {
        let db = ProductsDb::open_in_memory().unwrap();
        let id = db.add(&write("111", "MILK 1L", 12_050, 15_000, None)).unwrap();
        let p = db.get(id).unwrap().unwrap();
        assert_eq!((p.barcode.as_str(), p.name.as_str(), p.cost_cents, p.price_cents, p.gct), ("111", "MILK 1L", 12_050, 15_000, true));
        assert_eq!(p.membership, Membership::Standalone);
        assert_eq!(db.list(&ProductQuery { search: "milk".into(), limit: 10, offset: 0 }).unwrap().total, 1, "search index follows the insert");
        assert_eq!(db.get(9999).unwrap(), None);
    }

    #[test]
    fn duplicate_barcode_is_its_own_error() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.add(&write("111", "A", 100, 100, None)).unwrap();
        assert_eq!(db.add(&write("111", "B", 100, 100, None)), Err(ProductError::DuplicateBarcode("111".into())));
        let other = db.add(&write("222", "B", 100, 100, None)).unwrap();
        assert_eq!(db.update(other, &write("111", "B", 100, 100, None), 0.10), Err(ProductError::DuplicateBarcode("111".into())));
    }

    #[test]
    fn groups_are_listed_by_name_with_their_markup() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| c.execute_batch("INSERT INTO groups (name, profit_margin) VALUES ('MEATS', 0.35), ('GENERAL', 0.25);")).unwrap();
        let g = db.groups().unwrap();
        assert_eq!(g.iter().map(|g| (g.name.as_str(), g.markup)).collect::<Vec<_>>(), [("GENERAL", 0.25), ("MEATS", 0.35)]);
    }

    #[test]
    fn edit_changes_the_product_and_the_search_index() {
        let db = ProductsDb::open_in_memory().unwrap();
        let id = db.add(&write("111", "MILK 1L", 12_000, 15_000, None)).unwrap();
        let out = db.update(id, &write("999", "OAT MILK", 13_000, 16_900, None), 0.10).unwrap();
        assert!(out.repriced_cases.is_empty() && out.group_sync.is_none());
        let p = db.get(id).unwrap().unwrap();
        assert_eq!((p.barcode.as_str(), p.name.as_str(), p.cost_cents, p.price_cents), ("999", "OAT MILK", 13_000, 16_900));
        let q = |s: &str| db.list(&ProductQuery { search: s.into(), limit: 10, offset: 0 }).unwrap().total;
        assert_eq!((q("oat"), q("milk 1l"), q("999"), q("111")), (1, 0, 1, 0));
        assert_eq!(db.update(4242, &write("1", "X", 1, 1, None), 0.10), Err(ProductError::NotFound));
    }

    #[test]
    fn a_cost_change_flows_to_linked_cases_only_when_the_cost_changes() {
        let db = ProductsDb::open_in_memory().unwrap();
        let single = db.add(&write("111", "COLA", 100, 150, None)).unwrap();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO products (barcode, name, cost, selling_price, is_case, case_qty, case_product_id)
                 VALUES ('111C', 'COLA CASE', 24.0, 26.4, 1, 24, ?1)",
                [single],
            )
        })
        .unwrap();
        // name-only edit: the case is left alone
        assert!(db.update(single, &write("111", "COLA 330", 100, 150, None), 0.10).unwrap().repriced_cases.is_empty());
        // cost 1.00 -> 1.50: case = 24 x 1.50 = 36.00, price = 36.00 x 1.10 = 39.60
        assert_eq!(db.update(single, &write("111", "COLA 330", 150, 225, None), 0.10).unwrap().repriced_cases, ["COLA CASE"]);
        let (cost, price): (f64, f64) =
            db.with_conn(|c| c.query_row("SELECT cost, selling_price FROM products WHERE barcode='111C'", [], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
        assert_eq!((cost, price), (36.0, 39.6));
    }

    #[test]
    fn case_items_are_not_editable_yet() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| c.execute_batch("INSERT INTO products (barcode, name, is_case, case_qty) VALUES ('C1', 'BOX', 1, 6);")).unwrap();
        let id: i64 = db.with_conn(|c| c.query_row("SELECT id FROM products WHERE barcode='C1'", [], |r| r.get(0))).unwrap();
        assert!(matches!(db.update(id, &write("C1", "NEW", 1, 1, None), 0.10), Err(ProductError::Invalid(_))));
        assert_eq!(db.get(id).unwrap().unwrap().name, "BOX");
    }

    // ── alias / variant groups ──

    fn in_group(kind: GroupKind, gid: i64, barcode: &str, name: &str, cost: i64, price: i64) -> ProductWrite {
        let mut w = write(barcode, name, cost, price, None);
        match kind {
            GroupKind::Alias => w.alias_group_id = Some(gid),
            GroupKind::Variant => w.variant_group_id = Some(gid),
        }
        w
    }

    fn group_values(db: &ProductsDb, table: &str, gid: i64) -> (f64, f64) {
        db.with_conn(|c| c.query_row(&format!("SELECT cost, selling_price FROM {table} WHERE id=?1"), [gid], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap()
    }

    #[test]
    fn price_groups_are_listed_and_created_upper_case_without_duplicates() {
        let db = ProductsDb::open_in_memory().unwrap();
        let soda = db.create_price_group(GroupKind::Alias, " soda 45 ").unwrap();
        assert_eq!((soda.name.as_str(), soda.cost_cents, soda.price_cents), ("SODA 45", 0, 0));
        db.create_price_group(GroupKind::Alias, "JUICE").unwrap();
        db.create_price_group(GroupKind::Variant, "SODA 45").unwrap(); // the other kind is a separate list
        assert_eq!(db.price_groups(GroupKind::Alias).unwrap().iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["JUICE", "SODA 45"]);
        assert_eq!(db.price_groups(GroupKind::Variant).unwrap().len(), 1);
        assert_eq!(
            db.create_price_group(GroupKind::Alias, "soda 45"),
            Err(ProductError::Invalid("An alias group named \"SODA 45\" already exists.".into()))
        );
        assert!(db.create_price_group(GroupKind::Variant, "  ").is_err());
    }

    #[test]
    fn first_product_in_a_new_group_sets_the_groups_price_later_ones_follow_it() {
        let db = ProductsDb::open_in_memory().unwrap();
        let g = db.create_price_group(GroupKind::Alias, "SODA 45").unwrap().id;
        let first = db.add(&in_group(GroupKind::Alias, g, "1", "7UP", 3000, 4500)).unwrap();
        assert_eq!(group_values(&db, "alias_groups", g), (30.0, 45.0));
        let listed = db.price_groups(GroupKind::Alias).unwrap();
        assert_eq!((listed[0].cost_cents, listed[0].price_cents), (3000, 4500));
        // a second product joins with different numbers: the group keeps its own
        let second = db.add(&in_group(GroupKind::Alias, g, "2", "PEPSI", 100, 150)).unwrap();
        assert_eq!(group_values(&db, "alias_groups", g), (30.0, 45.0));
        for id in [first, second] {
            let p = db.get(id).unwrap().unwrap();
            assert_eq!((p.cost_cents, p.price_cents), (3000, 4500), "effective values come from the group");
            assert_eq!(p.membership, Membership::Alias("SODA 45".into()));
        }
    }

    #[test]
    fn changing_a_members_cost_changes_the_group_and_every_member_and_their_cases() {
        let db = ProductsDb::open_in_memory().unwrap();
        let g = db.create_price_group(GroupKind::Alias, "SODA 45").unwrap().id;
        let a = db.add(&in_group(GroupKind::Alias, g, "1", "7UP", 3000, 4500)).unwrap();
        let b = db.add(&in_group(GroupKind::Alias, g, "2", "PEPSI", 3000, 4500)).unwrap();
        db.with_conn(|c| {
            c.execute("INSERT INTO products (barcode, name, cost, selling_price, is_case, case_qty, case_product_id) VALUES ('2C', 'PEPSI CASE', 720, 792, 1, 24, ?1)", [b])
        })
        .unwrap();
        // name-only edit: nothing is pushed to the group
        let out = db.update(a, &in_group(GroupKind::Alias, g, "1", "7UP 500ML", 3000, 4500), 0.10).unwrap();
        assert_eq!(out, UpdateOutcome::default());
        // cost 30.00 -> 33.00, price 45.00 -> 49.50
        let out = db.update(a, &in_group(GroupKind::Alias, g, "1", "7UP 500ML", 3300, 4950), 0.10).unwrap();
        assert_eq!(out.group_sync, Some(GroupSync { group_name: "SODA 45".into(), cost_cents: 3300, price_cents: 4950 }));
        assert_eq!(out.repriced_cases, ["PEPSI CASE"], "the case of the OTHER member is repriced too");
        assert_eq!(group_values(&db, "alias_groups", g), (33.0, 49.5));
        let other = db.get(b).unwrap().unwrap();
        assert_eq!((other.cost_cents, other.price_cents), (3300, 4950));
        let case_cost: f64 = db.with_conn(|c| c.query_row("SELECT cost FROM products WHERE barcode='2C'", [], |r| r.get(0))).unwrap();
        assert_eq!(case_cost, 792.0); // 24 x 33.00
    }

    #[test]
    fn moving_a_product_into_a_priced_group_does_not_overwrite_it_but_an_empty_group_takes_the_price() {
        let db = ProductsDb::open_in_memory().unwrap();
        let priced = db.create_price_group(GroupKind::Variant, "CHIPS").unwrap().id;
        db.add(&in_group(GroupKind::Variant, priced, "1", "CHIPS BBQ", 10_000, 16_000)).unwrap();
        let empty = db.create_price_group(GroupKind::Variant, "NEW").unwrap().id;
        let id = db.add(&write("9", "LONER", 500, 800, None)).unwrap();
        db.update(id, &in_group(GroupKind::Variant, priced, "9", "LONER", 500, 800), 0.10).unwrap();
        assert_eq!(group_values(&db, "variant_groups", priced), (100.0, 160.0));
        db.update(id, &in_group(GroupKind::Variant, empty, "9", "LONER", 500, 800), 0.10).unwrap();
        assert_eq!(group_values(&db, "variant_groups", empty), (5.0, 8.0));
        // leaving the group: the product keeps the numbers the form held
        db.update(id, &write("9", "LONER", 500, 800, None), 0.10).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().membership, Membership::Standalone);
    }

    #[test]
    fn a_variant_groups_cost_reprices_cases_linked_to_the_group() {
        let db = ProductsDb::open_in_memory().unwrap();
        let g = db.create_price_group(GroupKind::Variant, "CHIPS").unwrap().id;
        let id = db.add(&in_group(GroupKind::Variant, g, "1", "CHIPS BBQ", 100, 160)).unwrap();
        db.with_conn(|c| {
            c.execute("INSERT INTO products (barcode, name, cost, selling_price, is_case, case_qty, case_variant_group_id) VALUES ('CC', 'CHIPS BOX', 1, 1, 1, 12, ?1)", [g])
        })
        .unwrap();
        let out = db.update(id, &in_group(GroupKind::Variant, g, "1", "CHIPS BBQ", 120, 190), 0.10).unwrap();
        assert_eq!(out.repriced_cases, ["CHIPS BOX"]);
        let (cost, price): (f64, f64) = db.with_conn(|c| c.query_row("SELECT cost, selling_price FROM products WHERE barcode='CC'", [], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
        assert_eq!((cost, price), (14.4, 15.84)); // 12 x 1.20, then +10 %
    }

    #[test]
    fn changing_the_markup_group_of_a_member_also_updates_the_group_price() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| c.execute_batch("INSERT INTO groups (name, profit_margin) VALUES ('GENERAL', 0.25), ('MEATS', 0.35);")).unwrap();
        let g = db.create_price_group(GroupKind::Alias, "SODA").unwrap().id;
        let mut w = in_group(GroupKind::Alias, g, "1", "7UP", 10_000, 12_500);
        w.group_id = Some(1);
        let id = db.add(&w).unwrap();
        w.group_id = Some(2);
        w.price_cents = 13_500;
        let out = db.update(id, &w, 0.10).unwrap();
        assert!(out.group_sync.is_some());
        assert_eq!(group_values(&db, "alias_groups", g), (100.0, 135.0));
    }

    #[test]
    fn delete_removes_the_product_and_its_search_entry() {
        let db = ProductsDb::open_in_memory().unwrap();
        let id = db.add(&write("111", "MILK", 1, 1, None)).unwrap();
        assert!(db.delete(id).unwrap());
        assert!(!db.delete(id).unwrap());
        assert_eq!(db.list(&ProductQuery { search: "milk".into(), limit: 10, offset: 0 }).unwrap().total, 0);
    }

    // ── discounts ──

    fn custom(min_qty: u32, percent: f64, starts: Option<&str>, ends: Option<&str>) -> Option<SlotSetting> {
        Some(SlotSetting { source: SlotSource::Custom { min_qty, percent }, starts: starts.map(Into::into), ends: ends.map(Into::into) })
    }

    #[test]
    fn discount_levels_are_read_as_percent_smallest_quantity_first() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| c.execute_batch("INSERT INTO discount_levels (name, min_qty, percent) VALUES ('LEVEL 2', 6, 0.1), ('LEVEL 1', 3, 0.05), ('ODD', 12, 0.07);")).unwrap();
        let levels = db.discount_levels().unwrap();
        assert_eq!(
            levels.iter().map(|l| (l.name.as_str(), l.min_qty, l.percent)).collect::<Vec<_>>(),
            [("LEVEL 1", 3, 5.0), ("LEVEL 2", 6, 10.0), ("ODD", 12, 7.0)],
            "0.07 x 100 must read as 7, not 7.000000000000001"
        );
    }

    #[test]
    fn discounts_round_trip_with_dates_and_stacking() {
        let db = ProductsDb::open_in_memory().unwrap();
        db.with_conn(|c| c.execute_batch("INSERT INTO discount_levels (name, min_qty, percent) VALUES ('LEVEL 1', 3, 0.05);")).unwrap();
        let level = db.discount_levels().unwrap()[0].id;
        let mut w = write("111", "MILK", 1000, 1500, None);
        w.discounts = DiscountSetting {
            slot1: Some(SlotSetting { source: SlotSource::Level(level), starts: None, ends: None }),
            slot2: custom(3, 35.0, Some("2026-12-01"), Some("2026-12-24")),
            stack: true,
        };
        let id = db.add(&w).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().discounts, w.discounts);

        // a custom first tier and no second tier; the level and the dates are cleared
        w.discounts = DiscountSetting { slot1: custom(5, 12.5, None, None), slot2: None, stack: false };
        db.update(id, &w, 0.10).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().discounts, w.discounts);
        let (lvl, d2): (Option<i64>, Option<String>) = db
            .with_conn(|c| c.query_row("SELECT discount_level1_id, discount2_starts FROM products WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))))
            .unwrap();
        assert_eq!((lvl, d2), (None, None));

        // choosing "None" really removes the discount (the Python dialog left an old custom one behind)
        w.discounts = DiscountSetting::default();
        db.update(id, &w, 0.10).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().discounts, DiscountSetting::default());
    }

    #[test]
    fn an_old_python_database_is_upgraded_in_place_and_keeps_its_data() {
        let path = std::env::temp_dir().join(format!("merchantpos-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            // what the Python app created: no Rust-only columns, a product with an inline discount
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO products (barcode, name, cost, selling_price, inline_discount1_qty, inline_discount1_pct)
                 VALUES ('111', 'OLD ITEM', 1.5, 2.25, 3, 5.0);",
            )
            .unwrap();
        }
        for _ in 0..2 {
            // opening twice: the upgrade must be repeatable
            let db = ProductsDb::open(&path).unwrap();
            let id = db.list(&ProductQuery { search: "old".into(), limit: 5, offset: 0 }).unwrap().rows[0].id;
            let p = db.get(id).unwrap().unwrap();
            assert_eq!((p.cost_cents, p.price_cents), (150, 225));
            assert_eq!(p.discounts.slot1, custom(3, 5.0, None, None));
            assert!(!p.discounts.stack);
            let cols: i64 = db
                .with_conn(|c| c.query_row("SELECT COUNT(*) FROM pragma_table_info('products') WHERE name IN ('discount1_starts','discount1_ends','discount2_starts','discount2_ends','discount_stack')", [], |r| r.get(0)))
                .unwrap();
            assert_eq!(cols, 5);
        }
        let _ = std::fs::remove_file(&path);
    }
}
