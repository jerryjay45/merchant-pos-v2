//! Inventory > Groups & Discounts: markup groups, the case profit, and the shared discount
//! levels. Rows are edited in place; Add and Delete write at once (after a question for Delete),
//! everything else is written by Save, which first tells you how many products a new markup will
//! reprice. The rules live in `pos_core::catalog`; this file moves them between page and database.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use pos_core::{
    markup_text, parse_level, parse_markup_percent, CatalogRepository, ConfigRepository, LevelInput, MarkupChange,
};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::{supervisor::Ctx, LevelRowData, MarkupRowData, SupervisorWindow};

/// The action behind the Yes/No box that is open (shared with the product dialog's boxes).
pub type ConfirmSlot = Rc<RefCell<Option<Box<dyn FnOnce(&SupervisorWindow)>>>>;

struct GroupEdit {
    id: i64,
    name: String,
    products: u32,
    /// The stored markup (a fraction) and what the box holds now.
    original: f64,
    text: String,
}

struct LevelEdit {
    id: i64,
    in_use: u32,
    original: LevelInput,
    name: String,
    qty: String,
    pct: String,
}

struct State {
    groups: Vec<GroupEdit>,
    levels: Vec<LevelEdit>,
    case_original: f64,
    case_text: String,
}

#[derive(Clone)]
struct Env {
    weak: Weak<SupervisorWindow>,
    ctx: Arc<Ctx>,
    st: Rc<RefCell<State>>,
    slot: ConfirmSlot,
}

/// Handle the window keeps to (re)load the page when it is opened.
#[derive(Clone)]
pub struct Page(Env);

impl Page {
    pub fn load(&self, win: &SupervisorWindow) {
        self.0.load(win);
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn same_markup(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

impl Env {
    fn with(&self, f: impl FnOnce(&Env, &SupervisorWindow)) {
        if let Some(win) = self.weak.upgrade() {
            f(self, &win);
        }
    }

    fn status(&self, win: &SupervisorWindow, ok: bool, text: &str) {
        win.set_groups_status_ok(ok);
        win.set_groups_status(text.into());
    }

    // ── showing ──

    fn case_profit(&self) -> f64 {
        self.ctx
            .config
            .get("case_profit_pct")
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.10)
    }

    fn load(&self, win: &SupervisorWindow) {
        let groups = self.ctx.products.markup_groups();
        let levels = self.ctx.products.level_rows();
        match (groups, levels) {
            (Ok(g), Ok(l)) => {
                let case = self.case_profit();
                *self.st.borrow_mut() = State {
                    groups: g
                        .into_iter()
                        .map(|r| GroupEdit { id: r.id, text: markup_text(r.markup), original: r.markup, name: r.name, products: r.products })
                        .collect(),
                    levels: l
                        .into_iter()
                        .map(|r| LevelEdit {
                            id: r.level.id,
                            in_use: r.in_use,
                            name: r.level.name.clone(),
                            qty: r.level.min_qty.to_string(),
                            pct: pos_core::fmt_percent(r.level.percent),
                            original: LevelInput { name: r.level.name, min_qty: r.level.min_qty, percent: r.level.percent },
                        })
                        .collect(),
                    case_original: case,
                    case_text: markup_text(case),
                };
                win.set_case_pct(self.st.borrow().case_text.clone().into());
                self.clear_status(win);
                self.show(win);
            }
            (Err(e), _) | (_, Err(e)) => self.status(win, false, &format!("Could not read the groups: {e}")),
        }
    }

    fn clear_status(&self, win: &SupervisorWindow) {
        self.status(win, true, "");
    }

    /// Rebuilds the rows from the state and refreshes the "unsaved changes" note.
    fn show(&self, win: &SupervisorWindow) {
        let st = self.st.borrow();
        let groups: Vec<MarkupRowData> = st
            .groups
            .iter()
            .map(|g| MarkupRowData {
                name: g.name.as_str().into(),
                markup: g.text.as_str().into(),
                products: if g.products == 0 { "no products".to_string() } else { plural(g.products, "product", "products") }.into(),
            })
            .collect();
        let levels: Vec<LevelRowData> = st
            .levels
            .iter()
            .map(|l| LevelRowData {
                name: l.name.as_str().into(),
                qty: l.qty.as_str().into(),
                pct: l.pct.as_str().into(),
                used: if l.in_use == 0 { "unused".to_string() } else { plural(l.in_use, "use", "uses") }.into(),
            })
            .collect();
        win.set_markup_rows(ModelRc::new(VecModel::from(groups)));
        win.set_level_rows(ModelRc::new(VecModel::from(levels)));
        drop(st);
        self.refresh_dirty(win);
    }

    fn is_dirty(&self) -> bool {
        let st = self.st.borrow();
        let groups = st.groups.iter().any(|g| !parse_markup_percent(&g.text).is_ok_and(|v| same_markup(v, g.original)));
        let levels = st.levels.iter().any(|l| {
            l.name.trim() != l.original.name
                || l.qty.trim() != l.original.min_qty.to_string()
                || !parse_level(&l.name, &l.qty, &l.pct).is_ok_and(|n| n == l.original)
        });
        let case = !parse_markup_percent(&st.case_text).is_ok_and(|v| same_markup(v, st.case_original));
        groups || levels || case
    }

    fn refresh_dirty(&self, win: &SupervisorWindow) {
        win.set_groups_dirty(self.is_dirty());
    }

    fn ask(&self, win: &SupervisorWindow, title: &str, msg: &str, button: &str, action: Box<dyn FnOnce(&SupervisorWindow)>) {
        *self.slot.borrow_mut() = Some(action);
        win.set_confirm_message(msg.into());
        win.set_confirm_button(button.into());
        win.set_confirm_cancel_visible(true);
        win.set_confirm_title(title.into()); // set last: a non-empty title opens the box
    }

    // ── groups ──

    fn add_group(&self, win: &SupervisorWindow) {
        match self.ctx.products.add_markup_group(&win.get_new_group_name()) {
            Ok(g) => {
                self.st.borrow_mut().groups.push(GroupEdit { id: g.id, text: markup_text(g.markup), original: g.markup, name: g.name, products: 0 });
                self.st.borrow_mut().groups.sort_by(|a, b| a.name.cmp(&b.name));
                win.set_new_group_name("".into());
                self.clear_status(win);
                self.show(win);
            }
            Err(e) => self.status(win, false, &e.to_string()),
        }
    }

    fn ask_delete_group(&self, win: &SupervisorWindow, index: usize) {
        let (id, name, products) = {
            let st = self.st.borrow();
            let Some(g) = st.groups.get(index) else { return };
            (g.id, g.name.clone(), g.products)
        };
        let uses = if products == 0 {
            "No products use it.".to_string()
        } else {
            format!(
                "{} use it. They keep their prices but lose the group, so they stop following its markup.",
                plural(products, "product", "products")
            )
        };
        let env = self.clone();
        self.ask(
            win,
            "Delete group",
            &format!("Delete the group '{name}'?\n\n{uses}"),
            "Delete",
            Box::new(move |win| match env.ctx.products.delete_markup_group(id) {
                Ok(()) => {
                    env.st.borrow_mut().groups.retain(|g| g.id != id);
                    env.clear_status(win);
                    env.show(win);
                }
                Err(e) => env.status(win, false, &format!("Could not delete the group: {e}")),
            }),
        );
    }

    // ── levels ──

    fn add_level(&self, win: &SupervisorWindow) {
        let input = match parse_level(&win.get_new_level_name(), &win.get_new_level_qty(), &win.get_new_level_pct()) {
            Ok(i) => i,
            Err(e) => return self.status(win, false, &e),
        };
        match self.ctx.products.add_level(&input) {
            Ok(row) => {
                {
                    let mut st = self.st.borrow_mut();
                    st.levels.push(LevelEdit {
                        id: row.level.id,
                        in_use: 0,
                        name: row.level.name.clone(),
                        qty: row.level.min_qty.to_string(),
                        pct: pos_core::fmt_percent(row.level.percent),
                        original: input,
                    });
                    st.levels.sort_by_key(|l| l.original.min_qty);
                }
                win.set_new_level_name("".into());
                win.set_new_level_qty("".into());
                win.set_new_level_pct("".into());
                self.clear_status(win);
                self.show(win);
            }
            Err(e) => self.status(win, false, &e.to_string()),
        }
    }

    fn ask_delete_level(&self, win: &SupervisorWindow, index: usize) {
        let (id, name, in_use) = {
            let st = self.st.borrow();
            let Some(l) = st.levels.get(index) else { return };
            (l.id, l.original.name.clone(), l.in_use)
        };
        let uses = if in_use == 0 {
            "Nothing uses it.".to_string()
        } else {
            format!("{} use it and will lose this discount.", plural(in_use, "product or group", "products and groups"))
        };
        let env = self.clone();
        self.ask(
            win,
            "Delete discount level",
            &format!("Delete the level '{name}'?\n\n{uses}"),
            "Delete",
            Box::new(move |win| match env.ctx.products.delete_level(id) {
                Ok(()) => {
                    env.st.borrow_mut().levels.retain(|l| l.id != id);
                    env.clear_status(win);
                    env.show(win);
                }
                Err(e) => env.status(win, false, &format!("Could not delete the level: {e}")),
            }),
        );
    }

    // ── case profit ──

    fn recalc_cases(&self, win: &SupervisorWindow) {
        let pct = match parse_markup_percent(&self.st.borrow().case_text) {
            Ok(p) => p,
            Err(e) => return self.status(win, false, &format!("Case profit: {e}")),
        };
        if let Err(e) = self.ctx.config.set("case_profit_pct", &pct.to_string()) {
            return self.status(win, false, &format!("Could not save the case profit: {e}"));
        }
        self.st.borrow_mut().case_original = pct;
        match self.ctx.products.recalc_all_cases(pct) {
            Ok(names) if names.is_empty() => {
                self.status(win, true, &format!("Case profit saved at {}%. No case needed repricing.", markup_text(pct)))
            }
            Ok(names) => self.status(
                win,
                true,
                &format!("Case profit saved at {}%. {} repriced.", markup_text(pct), plural(names.len() as u32, "case product", "case products")),
            ),
            Err(e) => self.status(win, false, &format!("Could not reprice the cases: {e}")),
        }
        self.refresh_dirty(win);
    }

    // ── save ──

    /// Checks every box, works out what changed, asks before repricing, then writes it.
    fn save(&self, win: &SupervisorWindow) {
        let st = self.st.borrow();
        let mut markups = Vec::new();
        for g in &st.groups {
            match parse_markup_percent(&g.text) {
                Ok(v) if same_markup(v, g.original) => {}
                Ok(v) => markups.push(MarkupChange { group_id: g.id, markup: v }),
                Err(e) => return self.status(win, false, &format!("{}: {e}", g.name)),
            }
        }
        let mut levels = Vec::new();
        for l in &st.levels {
            match parse_level(&l.name, &l.qty, &l.pct) {
                Ok(n) if n == l.original => {}
                Ok(n) => levels.push((l.id, n)),
                Err(e) => return self.status(win, false, &e),
            }
        }
        let case = match parse_markup_percent(&st.case_text) {
            Ok(v) if same_markup(v, st.case_original) => None,
            Ok(v) => Some(v),
            Err(e) => return self.status(win, false, &format!("Case profit: {e}")),
        };
        drop(st);
        if markups.is_empty() && levels.is_empty() && case.is_none() {
            return self.status(win, true, "Nothing to save: no changes.");
        }

        let to_reprice = match self.ctx.products.count_repriced_by_markups(&markups) {
            Ok(n) => n,
            Err(e) => return self.status(win, false, &format!("Could not check the prices: {e}")),
        };
        if to_reprice == 0 {
            return self.write(win, markups, levels, case);
        }
        let env = self.clone();
        self.ask(
            win,
            "Reprice products?",
            &format!(
                "The new markups will change the selling price of {}.\n\nProducts in alias or variant groups, and case products, are not changed by this.",
                plural(to_reprice, "product", "products")
            ),
            "Save and reprice",
            Box::new(move |win| env.write(win, markups, levels, case)),
        );
    }

    fn write(&self, win: &SupervisorWindow, markups: Vec<MarkupChange>, levels: Vec<(i64, LevelInput)>, case: Option<f64>) {
        // Levels first: they can be refused (a name already taken) before anything is repriced.
        for (id, input) in &levels {
            if let Err(e) = self.ctx.products.update_level(*id, input) {
                self.load(win);
                return self.status(win, false, &format!("{}: {e} Nothing else was saved.", input.name));
            }
        }
        let mut pct = self.case_profit();
        if let Some(c) = case {
            if let Err(e) = self.ctx.config.set("case_profit_pct", &c.to_string()) {
                self.load(win);
                return self.status(win, false, &format!("Could not save the case profit: {e}"));
            }
            pct = c;
        }
        let outcome = match self.ctx.products.save_markups(&markups, pct) {
            Ok(o) => o,
            Err(e) => {
                self.load(win);
                return self.status(win, false, &format!("Could not save the markups: {e}"));
            }
        };
        let mut cases = outcome.cases_repriced;
        if case.is_some() {
            if let Ok(names) = self.ctx.products.recalc_all_cases(pct) {
                for n in names {
                    if !cases.contains(&n) {
                        cases.push(n);
                    }
                }
            }
        }
        self.load(win);
        let mut parts = vec!["Saved.".to_string()];
        if outcome.products_repriced > 0 {
            parts.push(format!("{} repriced.", plural(outcome.products_repriced, "product", "products")));
        }
        if !cases.is_empty() {
            parts.push(format!("{} repriced.", plural(cases.len() as u32, "case product", "case products")));
        }
        self.status(win, true, &parts.join(" "));
    }
}

/// Connects the page to storage and returns the handle the window uses to load it.
pub fn wire(win: &SupervisorWindow, ctx: Arc<Ctx>, slot: ConfirmSlot) -> Page {
    let env = Env {
        weak: win.as_weak(),
        ctx,
        st: Rc::new(RefCell::new(State { groups: Vec::new(), levels: Vec::new(), case_original: 0.10, case_text: "10".into() })),
        slot,
    };

    let e = env.clone();
    win.on_group_markup_edited(move |i, t| {
        e.with(|e, w| {
            if let Some(g) = e.st.borrow_mut().groups.get_mut(i as usize) {
                g.text = t.to_string();
            }
            e.clear_status(w);
            e.refresh_dirty(w);
        })
    });
    let e = env.clone();
    win.on_delete_group(move |i| e.with(|e, w| e.ask_delete_group(w, i.max(0) as usize)));
    let e = env.clone();
    win.on_add_group(move || e.with(|e, w| e.add_group(w)));

    let e = env.clone();
    win.on_case_pct_edited(move || {
        e.with(|e, w| {
            e.st.borrow_mut().case_text = w.get_case_pct().to_string();
            e.clear_status(w);
            e.refresh_dirty(w);
        })
    });
    let e = env.clone();
    win.on_recalc_cases(move || {
        e.with(|e, w| {
            e.st.borrow_mut().case_text = w.get_case_pct().to_string();
            e.recalc_cases(w);
        })
    });

    let e = env.clone();
    win.on_level_edited(move |i, field, t| {
        e.with(|e, w| {
            if let Some(l) = e.st.borrow_mut().levels.get_mut(i as usize) {
                match field {
                    0 => l.name = t.to_string(),
                    1 => l.qty = t.to_string(),
                    _ => l.pct = t.to_string(),
                }
            }
            e.clear_status(w);
            e.refresh_dirty(w);
        })
    });
    let e = env.clone();
    win.on_delete_level(move |i| e.with(|e, w| e.ask_delete_level(w, i.max(0) as usize)));
    let e = env.clone();
    win.on_add_level(move || e.with(|e, w| e.add_level(w)));
    let e = env.clone();
    win.on_groups_save(move || e.with(|e, w| e.save(w)));

    Page(env)
}
