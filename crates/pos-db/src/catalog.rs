//! Groups & Discounts storage: markup groups, shared discount levels and the case repricing that
//! follows them. Lives in `products.db` next to the products it prices.

use pos_core::{
    case_pricing::Unit,
    catalog::{clean_group_name, CatalogRepository, LevelInput, LevelRow, MarkupChange, MarkupGroupRow, MarkupOutcome},
    discount::DiscountLevel,
    money::to_cents,
    price_from_markup,
    product_edit::{GroupKind, ProductError},
    repo::StorageError,
};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    products::{cascade_single, cases_where_all, dollars, group_unit, reprice_case, ProductsDb},
    storage,
};

/// (id, cost, selling price) in cents of the standalone products in a markup group. Members of
/// alias/variant groups keep their price on that group, and cases are priced from their single.
fn standalone_in_group(conn: &Connection, group_id: i64) -> Result<Vec<(i64, i64, i64)>, StorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, cost, selling_price FROM products
             WHERE group_id = ?1 AND alias_group_id IS NULL AND variant_group_id IS NULL AND is_case = 0",
        )
        .map_err(storage)?;
    let rows = stmt
        .query_map([group_id], |r| Ok((r.get(0)?, to_cents(r.get::<_, f64>(1)?), to_cents(r.get::<_, f64>(2)?))))
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    Ok(rows)
}

/// The cost and price a case takes from the product it is made of (a group member follows its group).
fn product_unit(conn: &Connection, id: i64) -> Result<Option<Unit>, StorageError> {
    conn.query_row(
        "SELECT COALESCE(vg.cost, ag.cost, p.cost), COALESCE(vg.selling_price, ag.selling_price, p.selling_price)
         FROM products p
         LEFT JOIN alias_groups   ag ON ag.id = p.alias_group_id
         LEFT JOIN variant_groups vg ON vg.id = p.variant_group_id
         WHERE p.id = ?1",
        [id],
        |r| Ok(Unit { cost_cents: to_cents(r.get::<_, f64>(0)?), price_cents: to_cents(r.get::<_, f64>(1)?) }),
    )
    .optional()
    .map_err(storage)
}

fn level_percent_stored(percent: f64) -> f64 {
    (percent * 10.0).round() / 1000.0 // 12.5 -> 0.125, 5 -> 0.05
}

impl CatalogRepository for ProductsDb {
    fn markup_groups(&self) -> Result<Vec<MarkupGroupRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT g.id, g.name, g.profit_margin, (SELECT COUNT(*) FROM products p WHERE p.group_id = g.id)
                 FROM groups g ORDER BY g.name",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(MarkupGroupRow { id: r.get(0)?, name: r.get(1)?, markup: r.get(2)?, products: r.get::<_, i64>(3)? as u32 })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn add_markup_group(&self, name: &str) -> Result<MarkupGroupRow, ProductError> {
        let name = clean_group_name(name)?;
        let conn = self.conn.lock().map_err(storage)?;
        let exists = conn
            .query_row("SELECT 1 FROM groups WHERE name = ?1", [&name], |_| Ok(()))
            .optional()
            .map_err(storage)?
            .is_some();
        if exists {
            return Err(ProductError::Invalid(format!("A group named \"{name}\" already exists.")));
        }
        conn.execute("INSERT INTO groups (name, profit_margin) VALUES (?1, 0)", [&name]).map_err(storage)?;
        Ok(MarkupGroupRow { id: conn.last_insert_rowid(), name, markup: 0.0, products: 0 })
    }

    fn delete_markup_group(&self, id: i64) -> Result<(), StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute("DELETE FROM groups WHERE id = ?1", [id]).map_err(storage)?;
        Ok(())
    }

    fn count_repriced_by_markups(&self, changes: &[MarkupChange]) -> Result<u32, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut n = 0;
        for ch in changes {
            for (_, cost, price) in standalone_in_group(&conn, ch.group_id)? {
                if price_from_markup(cost, ch.markup) != price {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    fn save_markups(&self, changes: &[MarkupChange], case_profit_pct: f64) -> Result<MarkupOutcome, StorageError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        let mut out = MarkupOutcome::default();
        let mut repriced: Vec<(i64, Unit)> = Vec::new();
        for ch in changes {
            tx.execute("UPDATE groups SET profit_margin = ?1 WHERE id = ?2", params![ch.markup, ch.group_id]).map_err(storage)?;
            for (id, cost, price) in standalone_in_group(&tx, ch.group_id)? {
                let new_price = price_from_markup(cost, ch.markup);
                if new_price != price {
                    tx.execute(
                        "UPDATE products SET selling_price = ?1, updated_at = datetime('now', 'localtime') WHERE id = ?2",
                        params![dollars(new_price), id],
                    )
                    .map_err(storage)?;
                    repriced.push((id, Unit { cost_cents: cost, price_cents: new_price }));
                }
            }
        }
        out.products_repriced = repriced.len() as u32;
        // A pack-discount case follows its single's price, so a repriced single reaches its cases.
        for (id, unit) in repriced {
            for name in cascade_single(&tx, id, unit, case_profit_pct)? {
                if !out.cases_repriced.contains(&name) {
                    out.cases_repriced.push(name);
                }
            }
        }
        tx.commit().map_err(storage)?;
        Ok(out)
    }

    fn level_rows(&self) -> Result<Vec<LevelRow>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare(
                "SELECT l.id, l.name, l.min_qty, l.percent,
                    (SELECT COUNT(*) FROM products p WHERE p.discount_level1_id = l.id OR p.discount_level2_id = l.id)
                  + (SELECT COUNT(*) FROM alias_groups a WHERE a.discount_level1_id = l.id OR a.discount_level2_id = l.id)
                  + (SELECT COUNT(*) FROM variant_groups v WHERE v.discount_level1_id = l.id OR v.discount_level2_id = l.id)
                 FROM discount_levels l ORDER BY l.min_qty, l.id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map([], |r| {
                let pct: f64 = r.get(3)?;
                Ok(LevelRow {
                    level: DiscountLevel {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        min_qty: r.get::<_, i64>(2)?.max(1) as u32,
                        percent: (pct * 100.0 * 1e4).round() / 1e4,
                    },
                    in_use: r.get::<_, i64>(4)? as u32,
                })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn add_level(&self, input: &LevelInput) -> Result<LevelRow, ProductError> {
        let conn = self.conn.lock().map_err(storage)?;
        if name_taken(&conn, &input.name, None)? {
            return Err(ProductError::Invalid(format!("A level named \"{}\" already exists.", input.name)));
        }
        conn.execute(
            "INSERT INTO discount_levels (name, min_qty, percent) VALUES (?1, ?2, ?3)",
            params![input.name, input.min_qty, level_percent_stored(input.percent)],
        )
        .map_err(storage)?;
        Ok(LevelRow {
            level: DiscountLevel { id: conn.last_insert_rowid(), name: input.name.clone(), min_qty: input.min_qty, percent: input.percent },
            in_use: 0,
        })
    }

    fn update_level(&self, id: i64, input: &LevelInput) -> Result<(), ProductError> {
        let conn = self.conn.lock().map_err(storage)?;
        if name_taken(&conn, &input.name, Some(id))? {
            return Err(ProductError::Invalid(format!("A level named \"{}\" already exists.", input.name)));
        }
        conn.execute(
            "UPDATE discount_levels SET name = ?1, min_qty = ?2, percent = ?3 WHERE id = ?4",
            params![input.name, input.min_qty, level_percent_stored(input.percent), id],
        )
        .map_err(storage)?;
        Ok(())
    }

    fn delete_level(&self, id: i64) -> Result<(), StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute("DELETE FROM discount_levels WHERE id = ?1", [id]).map_err(storage)?;
        Ok(())
    }

    fn recalc_all_cases(&self, case_profit_pct: f64) -> Result<Vec<String>, StorageError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        let mut names = Vec::new();
        for (case, product, group) in cases_where_all(&tx)? {
            let unit = match (product, group) {
                (Some(pid), _) => product_unit(&tx, pid)?,
                (None, Some(gid)) => Some(group_unit(&tx, GroupKind::Variant, gid)?),
                _ => None, // not linked to anything: the typed numbers stay
            };
            if let Some(unit) = unit {
                if reprice_case(&tx, &case, unit, case_profit_pct)? {
                    names.push(case.name.clone());
                }
            }
        }
        tx.commit().map_err(storage)?;
        Ok(names)
    }
}

fn name_taken(conn: &Connection, name: &str, except: Option<i64>) -> Result<bool, StorageError> {
    Ok(conn
        .query_row("SELECT 1 FROM discount_levels WHERE name = ?1 AND id <> ?2", params![name, except.unwrap_or(-1)], |_| Ok(()))
        .optional()
        .map_err(storage)?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pos_core::{
        case_pricing::{CaseLink, CaseSetting, PriceMode},
        discount::{SlotSetting, SlotSource},
        product::ProductRepository,
        product_edit::ProductWrite,
    };

    fn write(barcode: &str, name: &str, cost: i64, price: i64, group: Option<i64>) -> ProductWrite {
        ProductWrite {
            barcode: barcode.into(),
            name: name.into(),
            cost_cents: cost,
            price_cents: price,
            group_id: group,
            alias_group_id: None,
            variant_group_id: None,
            gct: true,
            discounts: Default::default(),
            case: None,
        }
    }

    fn input(name: &str, qty: u32, percent: f64) -> LevelInput {
        LevelInput { name: name.into(), min_qty: qty, percent }
    }

    fn price_of(db: &ProductsDb, barcode: &str) -> f64 {
        db.with_conn(|c| c.query_row("SELECT selling_price FROM products WHERE barcode=?1", [barcode], |r| r.get(0))).unwrap()
    }

    #[test]
    fn groups_are_listed_with_their_product_counts_created_upper_case_and_deleted_safely() {
        let db = ProductsDb::open_in_memory().unwrap();
        let g = db.add_markup_group(" meats ").unwrap();
        assert_eq!((g.name.as_str(), g.markup, g.products), ("MEATS", 0.0, 0));
        assert_eq!(db.add_markup_group("MEATS"), Err(ProductError::Invalid("A group named \"MEATS\" already exists.".into())));
        assert!(db.add_markup_group("  ").is_err());
        let id = db.add(&write("1", "STEAK", 1000, 1000, Some(g.id)), 0.10).unwrap();
        assert_eq!(db.markup_groups().unwrap()[0].products, 1);
        // deleting the group keeps the product and its price
        db.delete_markup_group(g.id).unwrap();
        assert!(db.markup_groups().unwrap().is_empty());
        let p = db.get(id).unwrap().unwrap();
        assert_eq!((p.group_id, p.price_cents), (None, 1000));
    }

    #[test]
    fn saving_a_markup_reprices_only_standalone_products_of_the_changed_groups() {
        let db = ProductsDb::open_in_memory().unwrap();
        let general = db.add_markup_group("GENERAL").unwrap().id;
        let meats = db.add_markup_group("MEATS").unwrap().id;
        db.save_markups(&[MarkupChange { group_id: general, markup: 0.25 }, MarkupChange { group_id: meats, markup: 0.35 }], 0.10).unwrap();
        db.add(&write("A", "COLA", 10_000, 12_500, Some(general)), 0.10).unwrap();
        db.add(&write("B", "WATER", 20_000, 25_000, Some(general)), 0.10).unwrap();
        db.add(&write("C", "STEAK", 10_000, 13_500, Some(meats)), 0.10).unwrap();
        // a product in the group whose price already equals the new markup isn't touched or counted
        db.add(&write("D", "JUICE", 10_000, 13_000, Some(general)), 0.10).unwrap();
        // an alias group member keeps the price of its group
        let alias = db.create_price_group(GroupKind::Alias, "SODA").unwrap().id;
        let mut member = write("E", "7UP", 10_000, 12_500, Some(general));
        member.alias_group_id = Some(alias);
        db.add(&member, 0.10).unwrap();

        let change = [MarkupChange { group_id: general, markup: 0.30 }];
        assert_eq!(db.count_repriced_by_markups(&change).unwrap(), 2, "COLA and WATER; JUICE already at 30 %; the alias member is skipped");
        let out = db.save_markups(&change, 0.10).unwrap();
        assert_eq!((out.products_repriced, out.cases_repriced.len()), (2, 0));
        assert_eq!((price_of(&db, "A"), price_of(&db, "B"), price_of(&db, "D")), (130.0, 260.0, 130.0));
        assert_eq!(price_of(&db, "C"), 135.0, "another group is untouched");
        assert_eq!(price_of(&db, "E"), 125.0, "alias member: the price lives on the alias group");
        assert_eq!(db.markup_groups().unwrap().iter().find(|g| g.id == general).unwrap().markup, 0.30);
        // saving the same markup again changes nothing
        assert_eq!(db.save_markups(&change, 0.10).unwrap(), MarkupOutcome::default());
    }

    #[test]
    fn a_repriced_single_reaches_the_cases_that_depend_on_its_price() {
        let db = ProductsDb::open_in_memory().unwrap();
        let g = db.add_markup_group("GENERAL").unwrap().id;
        db.save_markups(&[MarkupChange { group_id: g, markup: 0.25 }], 0.10).unwrap();
        let single = db.add(&write("S", "COLA", 100, 125, Some(g)), 0.10).unwrap();
        let case = |barcode: &str, name: &str, mode| {
            let mut w = write(barcode, name, 0, 0, None);
            w.case = Some(CaseSetting { link: Some(CaseLink::Product(single)), units: 12, mode });
            db.add(&w, 0.10).unwrap();
        };
        case("PK", "PACK CASE", PriceMode::PackDiscount { percent: 10.0 });
        case("MK", "MARKUP CASE", PriceMode::Markup);
        assert_eq!(price_of(&db, "PK"), 13.5); // 12 x $1.25 less 10 %
        let out = db.save_markups(&[MarkupChange { group_id: g, markup: 0.35 }], 0.10).unwrap();
        assert_eq!(out.products_repriced, 1);
        assert_eq!(out.cases_repriced, ["PACK CASE"], "the markup case depends on the cost, which didn't change");
        assert_eq!(price_of(&db, "S"), 1.35);
        assert_eq!(price_of(&db, "PK"), 14.58); // 12 x $1.35 less 10 %
    }

    #[test]
    fn levels_are_stored_as_fractions_listed_with_usage_and_deleted_safely() {
        let db = ProductsDb::open_in_memory().unwrap();
        let l1 = db.add_level(&input("Level 1", 3, 5.0)).unwrap();
        let l2 = db.add_level(&input("Level 2", 6, 12.5)).unwrap();
        let stored: f64 = db.with_conn(|c| c.query_row("SELECT percent FROM discount_levels WHERE id=?1", [l2.level.id], |r| r.get(0))).unwrap();
        assert_eq!(stored, 0.125);
        assert_eq!(db.add_level(&input("Level 1", 9, 9.0)), Err(ProductError::Invalid("A level named \"Level 1\" already exists.".into())));

        let mut w = write("1", "MILK", 100, 150, None);
        w.discounts.slot1 = Some(SlotSetting { source: SlotSource::Level(l1.level.id), starts: None, ends: None });
        let product = db.add(&w, 0.10).unwrap();
        let alias = db.create_price_group(GroupKind::Alias, "SODA").unwrap().id;
        db.with_conn(|c| c.execute("UPDATE alias_groups SET discount_level1_id = ?1 WHERE id = ?2", [l1.level.id, alias])).unwrap();
        let rows = db.level_rows().unwrap();
        assert_eq!(rows.iter().map(|r| (r.level.name.as_str(), r.level.min_qty, r.level.percent, r.in_use)).collect::<Vec<_>>(), [("Level 1", 3, 5.0, 2), ("Level 2", 6, 12.5, 0)]);

        // rename and change; the name can't collide with another level's
        db.update_level(l1.level.id, &input("Everyday", 4, 7.5)).unwrap();
        assert!(db.update_level(l1.level.id, &input("Level 2", 4, 7.5)).is_err());
        db.update_level(l1.level.id, &input("Everyday", 5, 8.0)).unwrap(); // keeping its own name is fine
        assert_eq!(db.level_rows().unwrap()[0].level.percent, 8.0);

        // deleting a level removes it from the products that used it
        db.delete_level(l1.level.id).unwrap();
        assert_eq!(db.level_rows().unwrap().len(), 1);
        assert_eq!(db.get(product).unwrap().unwrap().discounts.slot1, None);
    }

    #[test]
    fn recalculating_all_cases_follows_each_cases_own_price_mode() {
        let db = ProductsDb::open_in_memory().unwrap();
        let single = db.add(&write("S", "COLA", 100, 180, None), 0.10).unwrap();
        let add_case = |barcode: &str, name: &str, link: Option<CaseLink>, mode, cost: i64, price: i64| {
            let mut w = write(barcode, name, cost, price, None);
            w.case = Some(CaseSetting { link, units: 12, mode });
            db.add(&w, 0.10).unwrap();
        };
        add_case("M", "MARKUP CASE", Some(CaseLink::Product(single)), PriceMode::Markup, 0, 0);
        add_case("P", "PACK CASE", Some(CaseLink::Product(single)), PriceMode::PackDiscount { percent: 10.0 }, 0, 0);
        add_case("U", "LOOSE CASE", None, PriceMode::Markup, 2000, 2600);
        assert_eq!(price_of(&db, "M"), 13.2); // 12 x $1.00 + 10 %
        // the shop profit goes from 10 % to 20 %: only the "cost + profit" case follows
        let changed = db.recalc_all_cases(0.20).unwrap();
        assert_eq!(changed, ["MARKUP CASE"]);
        assert_eq!((price_of(&db, "M"), price_of(&db, "P"), price_of(&db, "U")), (14.4, 19.44, 26.0));
        assert!(db.recalc_all_cases(0.20).unwrap().is_empty(), "nothing left to change");
    }
}
