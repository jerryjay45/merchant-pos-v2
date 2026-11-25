//! Inventory > Groups & Discounts: markup groups, the case profit, and the shared discount
//! levels. Both lists work like the Products list: one page at a time, searched in the database,
//! loaded on a background thread (a late answer to an old request is dropped), so opening the
//! tab never loads everything, however many groups and levels there are.
//!
//! Rows are edited in place; only the rows you touch are remembered, so edits survive paging and
//! searching. Add and Delete write at once (Delete asks first); everything else is written by
//! Save, which first tells you how many products a new markup will reprice. The rules live in
//! `pos_core::catalog`; this file moves them between the page and the database.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex},
};

use pos_core::{
    markup_text, parse_level, parse_markup_percent, CatalogRepository, ConfigRepository, LevelInput, LevelRow, ListQuery,
    MarkupChange, MarkupGroupRow, Pager,
};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::{supervisor::Ctx, LevelRowData, MarkupRowData, SupervisorWindow};

/// Rows per page, in each list.
const PAGE_SIZE: u32 = 20;

/// The action behind the Yes/No box that is open (shared with the product dialog's boxes).
pub type ConfirmSlot = Rc<RefCell<Option<Box<dyn FnOnce(&SupervisorWindow)>>>>;

/// A markup box the user has changed.
struct GroupEdit {
    /// The stored markup (a fraction) it started from.
    original: f64,
    name: String,
    text: String,
}

/// A level the user has changed.
struct LevelEdit {
    original: LevelInput,
    name: String,
    qty: String,
    pct: String,
}

struct State {
    g_search: String,
    g_pager: Pager,
    g_token: u64,
    /// The rows on the page being shown.
    g_rows: Vec<MarkupGroupRow>,
    g_edits: HashMap<i64, GroupEdit>,

    l_search: String,
    l_pager: Pager,
    l_token: u64,
    l_rows: Vec<LevelRow>,
    l_edits: HashMap<i64, LevelEdit>,

    case_original: f64,
    case_text: String,
}

impl State {
    fn new() -> Self {
        Self {
            g_search: String::new(),
            g_pager: Pager::new(PAGE_SIZE),
            g_token: 0,
            g_rows: Vec::new(),
            g_edits: HashMap::new(),
            l_search: String::new(),
            l_pager: Pager::new(PAGE_SIZE),
            l_token: 0,
            l_rows: Vec::new(),
            l_edits: HashMap::new(),
            case_original: 0.10,
            case_text: "10".into(),
        }
    }

    fn is_dirty(&self) -> bool {
        let groups = self.g_edits.values().any(|e| !parse_markup_percent(&e.text).is_ok_and(|v| same(v, e.original)));
        let levels = self.l_edits.values().any(|e| !parse_level(&e.name, &e.qty, &e.pct).is_ok_and(|n| n == e.original));
        let case = !parse_markup_percent(&self.case_text).is_ok_and(|v| same(v, self.case_original));
        groups || levels || case
    }
}

type Shared = Arc<Mutex<State>>;

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ───────────────────────────── loading and showing ─────────────────────────────

fn load_groups(win: &SupervisorWindow, ctx: &Arc<Ctx>, st: &Shared) {
    let (query, token) = {
        let mut s = st.lock().unwrap();
        s.g_token += 1;
        (ListQuery { search: s.g_search.clone(), limit: PAGE_SIZE, offset: s.g_pager.offset() }, s.g_token)
    };
    win.set_group_loading(true);
    let (ctx, weak, st) = (ctx.clone(), win.as_weak(), st.clone());
    std::thread::spawn(move || {
        let result = ctx.products.markup_groups(&query);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if token != s.g_token {
                return; // a newer request replaced this one
            }
            win.set_group_loading(false);
            match result {
                Ok(page) => {
                    let before = s.g_pager.page;
                    s.g_pager.set_total(page.total);
                    if s.g_pager.page != before {
                        drop(s); // the list got shorter: fetch the last page that still exists
                        return load_groups(&win, &ctx, &st);
                    }
                    s.g_rows = page.rows;
                    show_groups(&win, &s);
                }
                Err(e) => set_status(&win, false, &format!("Could not read the groups: {e}")),
            }
        });
    });
}

fn load_levels(win: &SupervisorWindow, ctx: &Arc<Ctx>, st: &Shared) {
    let (query, token) = {
        let mut s = st.lock().unwrap();
        s.l_token += 1;
        (ListQuery { search: s.l_search.clone(), limit: PAGE_SIZE, offset: s.l_pager.offset() }, s.l_token)
    };
    win.set_level_loading(true);
    let (ctx, weak, st) = (ctx.clone(), win.as_weak(), st.clone());
    std::thread::spawn(move || {
        let result = ctx.products.level_rows(&query);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if token != s.l_token {
                return;
            }
            win.set_level_loading(false);
            match result {
                Ok(page) => {
                    let before = s.l_pager.page;
                    s.l_pager.set_total(page.total);
                    if s.l_pager.page != before {
                        drop(s);
                        return load_levels(&win, &ctx, &st);
                    }
                    s.l_rows = page.rows;
                    show_levels(&win, &s);
                }
                Err(e) => set_status(&win, false, &format!("Could not read the discount levels: {e}")),
            }
        });
    });
}

fn set_status(win: &SupervisorWindow, ok: bool, text: &str) {
    win.set_groups_status_ok(ok);
    win.set_groups_status(text.into());
}

fn show_groups(win: &SupervisorWindow, s: &State) {
    let rows: Vec<MarkupRowData> = s
        .g_rows
        .iter()
        .map(|g| MarkupRowData {
            name: g.name.as_str().into(),
            markup: s.g_edits.get(&g.id).map_or_else(|| markup_text(g.markup), |e| e.text.clone()).into(),
            products: if g.products == 0 { "no products".to_string() } else { plural(g.products, "product", "products") }.into(),
        })
        .collect();
    let shown = rows.len();
    win.set_markup_rows(ModelRc::new(VecModel::from(rows)));
    win.set_group_range(s.g_pager.range_label_of(shown, "group", "groups").into());
    win.set_group_can_prev(s.g_pager.has_prev());
    win.set_group_can_next(s.g_pager.has_next());
    win.set_group_empty(
        if s.g_search.trim().is_empty() {
            "No groups yet. Add one below, for example GENERAL at 25%.".to_string()
        } else {
            format!("No groups match “{}”.", s.g_search.trim())
        }
        .into(),
    );
    win.set_groups_dirty(s.is_dirty());
}

fn show_levels(win: &SupervisorWindow, s: &State) {
    let rows: Vec<LevelRowData> = s
        .l_rows
        .iter()
        .map(|r| {
            let (name, qty, pct) = match s.l_edits.get(&r.level.id) {
                Some(e) => (e.name.clone(), e.qty.clone(), e.pct.clone()),
                None => (r.level.name.clone(), r.level.min_qty.to_string(), pos_core::fmt_percent(r.level.percent)),
            };
            LevelRowData {
                name: name.into(),
                qty: qty.into(),
                pct: pct.into(),
                used: if r.in_use == 0 { "unused".to_string() } else { plural(r.in_use, "use", "uses") }.into(),
            }
        })
        .collect();
    let shown = rows.len();
    win.set_level_rows(ModelRc::new(VecModel::from(rows)));
    win.set_level_range(s.l_pager.range_label_of(shown, "level", "levels").into());
    win.set_level_can_prev(s.l_pager.has_prev());
    win.set_level_can_next(s.l_pager.has_next());
    win.set_level_empty(
        if s.l_search.trim().is_empty() {
            "No levels yet. Add one below, for example Level 1: 3 or more, 5% off.".to_string()
        } else {
            format!("No levels match “{}”.", s.l_search.trim())
        }
        .into(),
    );
    win.set_groups_dirty(s.is_dirty());
}

// ───────────────────────────── the page ─────────────────────────────

#[derive(Clone)]
struct Env {
    weak: Weak<SupervisorWindow>,
    ctx: Arc<Ctx>,
    st: Shared,
    slot: ConfirmSlot,
}

/// Handle the window keeps to (re)open the page.
#[derive(Clone)]
pub struct Page(Env);

impl Page {
    /// Opening the page starts fresh: first page of each list, nothing edited.
    pub fn load(&self, win: &SupervisorWindow) {
        self.0.open(win);
    }
}

impl Env {
    fn with(&self, f: impl FnOnce(&Env, &SupervisorWindow)) {
        if let Some(win) = self.weak.upgrade() {
            f(self, &win);
        }
    }

    fn case_profit(&self) -> f64 {
        self.ctx
            .config
            .get("case_profit_pct")
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.10)
    }

    fn open(&self, win: &SupervisorWindow) {
        let case = self.case_profit();
        {
            let mut s = self.st.lock().unwrap();
            let tokens = (s.g_token, s.l_token); // keep counting up so older answers stay dropped
            *s = State::new();
            s.g_token = tokens.0;
            s.l_token = tokens.1;
            s.case_original = case;
            s.case_text = markup_text(case);
        }
        win.set_group_search("".into());
        win.set_level_search("".into());
        win.set_case_pct(markup_text(case).into());
        set_status(win, true, "");
        load_groups(win, &self.ctx, &self.st);
        load_levels(win, &self.ctx, &self.st);
    }

    fn clear_status(&self, win: &SupervisorWindow) {
        set_status(win, true, "");
    }

    fn refresh_dirty(&self, win: &SupervisorWindow) {
        win.set_groups_dirty(self.st.lock().unwrap().is_dirty());
    }

    fn ask(&self, win: &SupervisorWindow, title: &str, msg: &str, button: &str, action: Box<dyn FnOnce(&SupervisorWindow)>) {
        *self.slot.borrow_mut() = Some(action);
        win.set_confirm_message(msg.into());
        win.set_confirm_button(button.into());
        win.set_confirm_cancel_visible(true);
        win.set_confirm_title(title.into()); // set last: a non-empty title opens the box
    }

    // ── groups ──

    fn group_edited(&self, win: &SupervisorWindow, index: usize, text: String) {
        {
            let mut s = self.st.lock().unwrap();
            let Some(row) = s.g_rows.get(index).cloned() else { return };
            let e = s.g_edits.entry(row.id).or_insert_with(|| GroupEdit { original: row.markup, name: row.name.clone(), text: String::new() });
            e.text = text;
            if parse_markup_percent(&e.text).is_ok_and(|v| same(v, e.original)) {
                s.g_edits.remove(&row.id); // back to what is stored: nothing to save
            }
        }
        self.clear_status(win);
        self.refresh_dirty(win);
    }

    fn group_search(&self, win: &SupervisorWindow, text: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.g_search = text.trim().to_string();
            s.g_pager.first();
        }
        load_groups(win, &self.ctx, &self.st);
    }

    fn group_turn(&self, win: &SupervisorWindow, forward: bool) {
        {
            let mut s = self.st.lock().unwrap();
            if forward { s.g_pager.next() } else { s.g_pager.prev() }
        }
        load_groups(win, &self.ctx, &self.st);
    }

    fn add_group(&self, win: &SupervisorWindow) {
        match self.ctx.products.add_markup_group(&win.get_new_group_name()) {
            Ok(g) => {
                win.set_new_group_name("".into());
                // Show it: search for it, so the new row is on the page.
                win.set_group_search(g.name.as_str().into());
                self.group_search(win, &g.name);
                set_status(win, true, &format!("Added {}. Set its markup, then Save.", g.name));
            }
            Err(e) => set_status(win, false, &e.to_string()),
        }
    }

    fn ask_delete_group(&self, win: &SupervisorWindow, index: usize) {
        let Some(row) = self.st.lock().unwrap().g_rows.get(index).cloned() else { return };
        let uses = if row.products == 0 {
            "No products use it.".to_string()
        } else {
            format!(
                "{} use it. They keep their prices but lose the group, so they stop following its markup.",
                plural(row.products, "product", "products")
            )
        };
        let env = self.clone();
        self.ask(
            win,
            "Delete group",
            &format!("Delete the group '{}'?\n\n{uses}", row.name),
            "Delete",
            Box::new(move |win| match env.ctx.products.delete_markup_group(row.id) {
                Ok(()) => {
                    env.st.lock().unwrap().g_edits.remove(&row.id);
                    env.clear_status(win);
                    load_groups(win, &env.ctx, &env.st);
                }
                Err(e) => set_status(win, false, &format!("Could not delete the group: {e}")),
            }),
        );
    }

    // ── levels ──

    fn level_edited(&self, win: &SupervisorWindow, index: usize, field: i32, text: String) {
        {
            let mut s = self.st.lock().unwrap();
            let Some(row) = s.l_rows.get(index).cloned() else { return };
            let original = LevelInput { name: row.level.name.clone(), min_qty: row.level.min_qty, percent: row.level.percent };
            let e = s.l_edits.entry(row.level.id).or_insert_with(|| LevelEdit {
                name: original.name.clone(),
                qty: original.min_qty.to_string(),
                pct: pos_core::fmt_percent(original.percent),
                original,
            });
            match field {
                0 => e.name = text,
                1 => e.qty = text,
                _ => e.pct = text,
            }
            if parse_level(&e.name, &e.qty, &e.pct).is_ok_and(|n| n == e.original) {
                s.l_edits.remove(&row.level.id);
            }
        }
        self.clear_status(win);
        self.refresh_dirty(win);
    }

    fn level_search(&self, win: &SupervisorWindow, text: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.l_search = text.trim().to_string();
            s.l_pager.first();
        }
        load_levels(win, &self.ctx, &self.st);
    }

    fn level_turn(&self, win: &SupervisorWindow, forward: bool) {
        {
            let mut s = self.st.lock().unwrap();
            if forward { s.l_pager.next() } else { s.l_pager.prev() }
        }
        load_levels(win, &self.ctx, &self.st);
    }

    fn add_level(&self, win: &SupervisorWindow) {
        let input = match parse_level(&win.get_new_level_name(), &win.get_new_level_qty(), &win.get_new_level_pct()) {
            Ok(i) => i,
            Err(e) => return set_status(win, false, &e),
        };
        match self.ctx.products.add_level(&input) {
            Ok(row) => {
                win.set_new_level_name("".into());
                win.set_new_level_qty("".into());
                win.set_new_level_pct("".into());
                win.set_level_search(row.level.name.as_str().into());
                self.level_search(win, &row.level.name);
                set_status(win, true, &format!("Added {}.", row.level.name));
            }
            Err(e) => set_status(win, false, &e.to_string()),
        }
    }

    fn ask_delete_level(&self, win: &SupervisorWindow, index: usize) {
        let Some(row) = self.st.lock().unwrap().l_rows.get(index).cloned() else { return };
        let uses = if row.in_use == 0 {
            "Nothing uses it.".to_string()
        } else {
            format!("{} use it and will lose this discount.", plural(row.in_use, "product or group", "products and groups"))
        };
        let env = self.clone();
        self.ask(
            win,
            "Delete discount level",
            &format!("Delete the level '{}'?\n\n{uses}", row.level.name),
            "Delete",
            Box::new(move |win| match env.ctx.products.delete_level(row.level.id) {
                Ok(()) => {
                    env.st.lock().unwrap().l_edits.remove(&row.level.id);
                    env.clear_status(win);
                    load_levels(win, &env.ctx, &env.st);
                }
                Err(e) => set_status(win, false, &format!("Could not delete the level: {e}")),
            }),
        );
    }

    // ── case profit ──

    fn recalc_cases(&self, win: &SupervisorWindow) {
        let pct = match parse_markup_percent(&self.st.lock().unwrap().case_text) {
            Ok(p) => p,
            Err(e) => return set_status(win, false, &format!("Case profit: {e}")),
        };
        if let Err(e) = self.ctx.config.set("case_profit_pct", &pct.to_string()) {
            return set_status(win, false, &format!("Could not save the case profit: {e}"));
        }
        self.st.lock().unwrap().case_original = pct;
        match self.ctx.products.recalc_all_cases(pct) {
            Ok(names) if names.is_empty() => {
                set_status(win, true, &format!("Case profit saved at {}%. No case needed repricing.", markup_text(pct)))
            }
            Ok(names) => set_status(
                win,
                true,
                &format!("Case profit saved at {}%. {} repriced.", markup_text(pct), plural(names.len() as u32, "case product", "case products")),
            ),
            Err(e) => set_status(win, false, &format!("Could not reprice the cases: {e}")),
        }
        self.refresh_dirty(win);
    }

    // ── save ──

    /// Checks every changed box (on any page), asks before repricing, then writes it.
    fn save(&self, win: &SupervisorWindow) {
        let (markups, levels, case) = {
            let s = self.st.lock().unwrap();
            let mut markups = Vec::new();
            for (id, e) in &s.g_edits {
                match parse_markup_percent(&e.text) {
                    Ok(v) if same(v, e.original) => {}
                    Ok(v) => markups.push(MarkupChange { group_id: *id, markup: v }),
                    Err(err) => return set_status(win, false, &format!("{}: {err}", e.name)),
                }
            }
            let mut levels = Vec::new();
            for (id, e) in &s.l_edits {
                match parse_level(&e.name, &e.qty, &e.pct) {
                    Ok(n) if n == e.original => {}
                    Ok(n) => levels.push((*id, n)),
                    Err(err) => return set_status(win, false, &err),
                }
            }
            let case = match parse_markup_percent(&s.case_text) {
                Ok(v) if same(v, s.case_original) => None,
                Ok(v) => Some(v),
                Err(err) => return set_status(win, false, &format!("Case profit: {err}")),
            };
            (markups, levels, case)
        };
        if markups.is_empty() && levels.is_empty() && case.is_none() {
            return set_status(win, true, "Nothing to save: no changes.");
        }

        let to_reprice = match self.ctx.products.count_repriced_by_markups(&markups) {
            Ok(n) => n,
            Err(e) => return set_status(win, false, &format!("Could not check the prices: {e}")),
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
                return set_status(win, false, &format!("{}: {e} Nothing else was saved.", input.name));
            }
            self.st.lock().unwrap().l_edits.remove(id);
        }
        let mut pct = self.case_profit();
        if let Some(c) = case {
            if let Err(e) = self.ctx.config.set("case_profit_pct", &c.to_string()) {
                return set_status(win, false, &format!("Could not save the case profit: {e}"));
            }
            pct = c;
            self.st.lock().unwrap().case_original = c;
        }
        let outcome = match self.ctx.products.save_markups(&markups, pct) {
            Ok(o) => o,
            Err(e) => return set_status(win, false, &format!("Could not save the markups: {e}")),
        };
        self.st.lock().unwrap().g_edits.clear();
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
        // show what is stored now (same page, same search)
        load_groups(win, &self.ctx, &self.st);
        load_levels(win, &self.ctx, &self.st);
        let mut parts = vec!["Saved.".to_string()];
        if outcome.products_repriced > 0 {
            parts.push(format!("{} repriced.", plural(outcome.products_repriced, "product", "products")));
        }
        if !cases.is_empty() {
            parts.push(format!("{} repriced.", plural(cases.len() as u32, "case product", "case products")));
        }
        set_status(win, true, &parts.join(" "));
    }
}

/// Connects the page to storage and returns the handle the window uses to open it.
pub fn wire(win: &SupervisorWindow, ctx: Arc<Ctx>, slot: ConfirmSlot) -> Page {
    let env = Env { weak: win.as_weak(), ctx, st: Arc::new(Mutex::new(State::new())), slot };

    let e = env.clone();
    win.on_group_markup_edited(move |i, t| e.with(|e, w| e.group_edited(w, i.max(0) as usize, t.to_string())));
    let e = env.clone();
    win.on_delete_group(move |i| e.with(|e, w| e.ask_delete_group(w, i.max(0) as usize)));
    let e = env.clone();
    win.on_add_group(move || e.with(|e, w| e.add_group(w)));
    let e = env.clone();
    win.on_group_search_submitted(move || e.with(|e, w| e.group_search(w, &w.get_group_search())));
    let e = env.clone();
    win.on_group_search_cleared(move || {
        e.with(|e, w| {
            w.set_group_search("".into());
            e.group_search(w, "");
        })
    });
    let e = env.clone();
    win.on_group_prev(move || e.with(|e, w| e.group_turn(w, false)));
    let e = env.clone();
    win.on_group_next(move || e.with(|e, w| e.group_turn(w, true)));

    let e = env.clone();
    win.on_case_pct_edited(move || {
        e.with(|e, w| {
            e.st.lock().unwrap().case_text = w.get_case_pct().to_string();
            e.clear_status(w);
            e.refresh_dirty(w);
        })
    });
    let e = env.clone();
    win.on_recalc_cases(move || {
        e.with(|e, w| {
            e.st.lock().unwrap().case_text = w.get_case_pct().to_string();
            e.recalc_cases(w);
        })
    });

    let e = env.clone();
    win.on_level_edited(move |i, field, t| e.with(|e, w| e.level_edited(w, i.max(0) as usize, field, t.to_string())));
    let e = env.clone();
    win.on_delete_level(move |i| e.with(|e, w| e.ask_delete_level(w, i.max(0) as usize)));
    let e = env.clone();
    win.on_add_level(move || e.with(|e, w| e.add_level(w)));
    let e = env.clone();
    win.on_level_search_submitted(move || e.with(|e, w| e.level_search(w, &w.get_level_search())));
    let e = env.clone();
    win.on_level_search_cleared(move || {
        e.with(|e, w| {
            w.set_level_search("".into());
            e.level_search(w, "");
        })
    });
    let e = env.clone();
    win.on_level_prev(move || e.with(|e, w| e.level_turn(w, false)));
    let e = env.clone();
    win.on_level_next(move || e.with(|e, w| e.level_turn(w, true)));
    let e = env.clone();
    win.on_groups_save(move || e.with(|e, w| e.save(w)));

    Page(env)
}
