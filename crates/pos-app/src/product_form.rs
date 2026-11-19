//! The Add / Edit Product dialog, the alias / variant group chooser and the Delete confirmation
//! on the Products page. The rules (price from markup, required fields, the barcode warning)
//! live in `pos_core::product_edit`; this file only moves them between the window and the
//! database.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use pos_core::{
    fmt_percent, format_money, parse_cost, parse_date, price_from_markup, price_hint, warnings_for_edit, ConfigRepository,
    DiscountLevel, DiscountSetting, Group, GroupKind, Membership, PriceGroup, ProductDetail, ProductDraft,
    ProductRepository, SlotSetting, SlotSource,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::{supervisor::Ctx, SupervisorWindow};

/// What the Yes/No box that is currently open is asking.
enum Pending {
    None,
    Delete { id: i64, close_form: bool },
    BarcodeChange,
    /// Only an OK button: nothing to decide.
    Notice,
}

/// The cost and price a member of a priced alias/variant group started with. While the cost and
/// the markup group are unchanged the form shows the group's price, not a recalculated one.
struct Baseline {
    kind: GroupKind,
    group_name: String,
    cost_cents: i64,
    price_cents: i64,
    markup_group: Option<i64>,
}

struct Chooser {
    kind: GroupKind,
    /// The names currently listed, in order (the search filter applied).
    shown: Vec<PriceGroup>,
}

struct Form {
    /// The product being edited (`None` = adding).
    editing: Option<ProductDetail>,
    groups: Vec<Group>,
    /// The shared discount levels offered in the two discount boxes.
    levels: Vec<DiscountLevel>,
    alias_id: Option<i64>,
    variant_id: Option<i64>,
    baseline: Option<Baseline>,
    chooser: Option<Chooser>,
    pending: Pending,
    symbol: String,
}

#[derive(Clone)]
struct Env {
    weak: Weak<SupervisorWindow>,
    ctx: Arc<Ctx>,
    form: Rc<RefCell<Form>>,
    /// Reloads the product list.
    reload: Rc<dyn Fn()>,
}

fn cost_text(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn kind_title(kind: GroupKind) -> &'static str {
    match kind {
        GroupKind::Alias => "Alias Group",
        GroupKind::Variant => "Variant Group",
    }
}

fn not_editable_notice(m: &Membership) -> String {
    match m {
        Membership::Case => "This product is a case item. Its cost and price come from the product or group it is \
                             a case of, and editing case items isn't available in the Rust version yet. Use the \
                             Python app for now."
            .to_string(),
        _ => String::new(),
    }
}

fn list(repo: &dyn ProductRepository, kind: GroupKind) -> Vec<PriceGroup> {
    repo.price_groups(kind).unwrap_or_default()
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl Env {
    // ── discounts ──

    /// One discount box -> the setting to store. Box 0 = none, the last entry = custom.
    fn read_slot(&self, win: &SupervisorWindow, n: u8) -> Result<Option<SlotSetting>, String> {
        let (index, qty, pct, from, to) = if n == 1 {
            (win.get_form_d1_index(), win.get_form_d1_qty(), win.get_form_d1_pct(), win.get_form_d1_from(), win.get_form_d1_to())
        } else {
            (win.get_form_d2_index(), win.get_form_d2_qty(), win.get_form_d2_pct(), win.get_form_d2_from(), win.get_form_d2_to())
        };
        if index <= 0 {
            return Ok(None);
        }
        let levels = self.form.borrow().levels.clone();
        let source = if index as usize == levels.len() + 1 {
            let min_qty = qty
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("Discount {n}: enter the minimum quantity as a whole number, like 3."))?;
            let percent = pct
                .trim()
                .trim_end_matches('%')
                .replace(',', ".")
                .parse::<f64>()
                .map_err(|_| format!("Discount {n}: enter the percentage as a number, like 35 or 12.5."))?;
            SlotSource::Custom { min_qty, percent }
        } else {
            match levels.get(index as usize - 1) {
                Some(l) => SlotSource::Level(l.id),
                None => return Ok(None),
            }
        };
        let date = |t: &str| parse_date(t).map_err(|e| format!("Discount {n}: {e}"));
        Ok(Some(SlotSetting { source, starts: date(&from)?, ends: date(&to)? }))
    }

    fn read_discounts(&self, win: &SupervisorWindow) -> Result<DiscountSetting, String> {
        Ok(DiscountSetting { slot1: self.read_slot(win, 1)?, slot2: self.read_slot(win, 2)?, stack: win.get_form_stack() })
    }

    fn fill_slot(&self, win: &SupervisorWindow, n: u8, slot: &Option<SlotSetting>, levels: &[DiscountLevel]) {
        let custom_index = levels.len() as i32 + 1;
        let (index, qty, pct, from, to) = match slot {
            None => (0, String::new(), String::new(), String::new(), String::new()),
            Some(s) => {
                let (index, qty, pct) = match &s.source {
                    SlotSource::Level(id) => (levels.iter().position(|l| l.id == *id).map_or(0, |i| i as i32 + 1), String::new(), String::new()),
                    SlotSource::Custom { min_qty, percent } => (custom_index, min_qty.to_string(), fmt_percent(*percent)),
                };
                (index, qty, pct, s.starts.clone().unwrap_or_default(), s.ends.clone().unwrap_or_default())
            }
        };
        if n == 1 {
            win.set_form_d1_index(index);
            win.set_form_d1_qty(qty.into());
            win.set_form_d1_pct(pct.into());
            win.set_form_d1_from(from.into());
            win.set_form_d1_to(to.into());
        } else {
            win.set_form_d2_index(index);
            win.set_form_d2_qty(qty.into());
            win.set_form_d2_pct(pct.into());
            win.set_form_d2_from(from.into());
            win.set_form_d2_to(to.into());
        }
    }

    /// The plain-language lines under the discount boxes (blank while an entry is incomplete).
    fn refresh_discount_summary(&self, win: &SupervisorWindow) {
        let text = match self.read_discounts(win) {
            Ok(d) if d.validate().is_ok() => d.summary(&self.form.borrow().levels, &today()).join("\n"),
            _ => String::new(),
        };
        win.set_form_discount_summary(text.into());
    }

    fn with(&self, f: impl FnOnce(&Env, &SupervisorWindow)) {
        if let Some(win) = self.weak.upgrade() {
            f(self, &win);
        }
    }

    fn ask(&self, win: &SupervisorWindow, pending: Pending, title: &str, msg: &str, button: &str, can_cancel: bool) {
        self.form.borrow_mut().pending = pending;
        win.set_confirm_message(msg.into());
        win.set_confirm_button(button.into());
        win.set_confirm_cancel_visible(can_cancel);
        win.set_confirm_title(title.into()); // set last: a non-empty title opens the box
    }

    fn close_confirm(&self, win: &SupervisorWindow) -> Pending {
        win.set_confirm_title("".into());
        std::mem::replace(&mut self.form.borrow_mut().pending, Pending::None)
    }

    fn currency(&self) -> String {
        self.ctx
            .config
            .get("currency_symbol")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "$".into())
    }

    fn group_markup(&self, index: i32) -> Option<f64> {
        let f = self.form.borrow();
        usize::try_from(index - 1).ok().and_then(|i| f.groups.get(i)).map(|g| g.markup)
    }

    fn markup_group_id(&self, index: i32) -> Option<i64> {
        let f = self.form.borrow();
        usize::try_from(index - 1).ok().and_then(|i| f.groups.get(i)).map(|g| g.id)
    }

    /// Does the form still hold exactly what the group has? Then the group's price applies.
    fn at_baseline(&self, win: &SupervisorWindow, cost_cents: i64) -> bool {
        let f = self.form.borrow();
        f.baseline
            .as_ref()
            .is_some_and(|b| b.cost_cents == cost_cents && b.markup_group == self.markup_group_id_locked(&f, win.get_form_group_index()))
    }

    fn markup_group_id_locked(&self, f: &Form, index: i32) -> Option<i64> {
        usize::try_from(index - 1).ok().and_then(|i| f.groups.get(i)).map(|g| g.id)
    }

    /// Live selling price under the cost box.
    fn recalc(&self, win: &SupervisorWindow) {
        let symbol = self.form.borrow().symbol.clone();
        match parse_cost(&win.get_form_cost()) {
            Ok(cost) => {
                if self.at_baseline(win, cost) {
                    let f = self.form.borrow();
                    let b = f.baseline.as_ref().expect("checked by at_baseline");
                    win.set_form_price_text(format_money(b.price_cents, &symbol).into());
                    win.set_form_price_hint(
                        format!(
                            "From the {} group {}. Change the cost to change the whole group.",
                            b.kind.label(),
                            b.group_name
                        )
                        .into(),
                    );
                    return;
                }
                let markup = self.group_markup(win.get_form_group_index()).unwrap_or(0.0);
                win.set_form_price_text(format_money(price_from_markup(cost, markup), &symbol).into());
                let mut hint = price_hint(cost, markup, &symbol);
                if let Some(b) = &self.form.borrow().baseline {
                    if !hint.is_empty() {
                        hint.push_str("   ·   ");
                    }
                    hint.push_str(&format!("Saving updates the {} group {}.", b.kind.label(), b.group_name));
                }
                win.set_form_price_hint(hint.into());
            }
            Err(_) => {
                win.set_form_price_text("".into());
                win.set_form_price_hint("".into());
            }
        }
    }

    fn group_name(&self, kind: GroupKind, id: Option<i64>) -> String {
        let Some(id) = id else { return String::new() };
        list(&*self.ctx.products, kind).into_iter().find(|g| g.id == id).map(|g| g.name).unwrap_or_default()
    }

    fn show_group_names(&self, win: &SupervisorWindow) {
        let (a, v) = {
            let f = self.form.borrow();
            (f.alias_id, f.variant_id)
        };
        win.set_form_alias_name(self.group_name(GroupKind::Alias, a).into());
        win.set_form_variant_name(self.group_name(GroupKind::Variant, v).into());
    }

    fn open(&self, win: &SupervisorWindow, editing: Option<ProductDetail>) {
        let groups = match self.ctx.products.groups() {
            Ok(g) => g,
            Err(e) => {
                win.set_error_text(format!("Could not read the product groups: {e}").into());
                return;
            }
        };
        let mut names = vec![SharedString::from("— No Group —")];
        names.extend(groups.iter().map(|g| SharedString::from(g.name.as_str())));
        win.set_form_group_names(ModelRc::new(VecModel::from(names)));

        let levels = self.ctx.products.discount_levels().unwrap_or_default();
        let mut options = vec![SharedString::from("— None —")];
        options.extend(levels.iter().map(|l| SharedString::from(l.label())));
        options.push(SharedString::from("Custom…"));
        win.set_form_discount_options(ModelRc::new(VecModel::from(options)));
        win.set_form_custom_index(levels.len() as i32 + 1);
        let discounts = editing.as_ref().map(|p| p.discounts.clone()).unwrap_or_default();
        self.fill_slot(win, 1, &discounts.slot1, &levels);
        self.fill_slot(win, 2, &discounts.slot2, &levels);
        win.set_form_stack(discounts.stack);

        let (title, notice, barcode, name, cost, gct, group_index) = match &editing {
            Some(p) => (
                "Edit Product",
                not_editable_notice(&p.membership),
                p.barcode.clone(),
                p.name.clone(),
                cost_text(p.cost_cents),
                p.gct,
                p.group_id.and_then(|id| groups.iter().position(|g| g.id == id)).map_or(0, |i| i as i32 + 1),
            ),
            None => ("Add Product", String::new(), String::new(), String::new(), String::new(), true, 0),
        };
        let baseline = editing.as_ref().and_then(|p| match &p.membership {
            Membership::Alias(n) => Some((GroupKind::Alias, n.clone())),
            Membership::Variant(n) => Some((GroupKind::Variant, n.clone())),
            _ => None,
        });
        {
            let mut f = self.form.borrow_mut();
            f.alias_id = editing.as_ref().and_then(|p| p.alias_group_id);
            f.variant_id = editing.as_ref().and_then(|p| p.variant_group_id);
            f.baseline = match (&editing, baseline) {
                (Some(p), Some((kind, group_name))) => Some(Baseline {
                    kind,
                    group_name,
                    cost_cents: p.cost_cents,
                    price_cents: p.price_cents,
                    markup_group: p.group_id,
                }),
                _ => None,
            };
            f.editing = editing;
            f.groups = groups;
            f.levels = levels;
            f.chooser = None;
            f.pending = Pending::None;
            f.symbol = self.currency();
        }
        win.set_form_editing(self.form.borrow().editing.is_some());
        win.set_form_title(title.into());
        win.set_form_notice(notice.into());
        win.set_form_barcode(barcode.into());
        win.set_form_name(name.into());
        win.set_form_cost(cost.into());
        win.set_form_gct(gct);
        win.set_form_group_index(group_index);
        win.set_form_error("".into());
        self.show_group_names(win);
        self.recalc(win);
        self.refresh_discount_summary(win);
        win.set_form_open(true);
    }

    fn open_edit(&self, win: &SupervisorWindow, id: i64) {
        match self.ctx.products.get(id) {
            Ok(Some(p)) => self.open(win, Some(p)),
            Ok(None) => {
                win.set_error_text("That product no longer exists.".into());
                (self.reload)();
            }
            Err(e) => win.set_error_text(format!("Could not open the product: {e}").into()),
        }
    }

    // ── group chooser ──

    fn open_chooser(&self, win: &SupervisorWindow, kind: GroupKind) {
        self.form.borrow_mut().chooser = Some(Chooser { kind, shown: Vec::new() });
        win.set_chooser_search("".into());
        win.set_chooser_error("".into());
        win.set_chooser_current(
            self.group_name(kind, {
                let f = self.form.borrow();
                match kind {
                    GroupKind::Alias => f.alias_id,
                    GroupKind::Variant => f.variant_id,
                }
            })
            .into(),
        );
        self.refilter(win, "");
        win.set_chooser_title(format!("Select {}", kind_title(kind)).into()); // set last: opens it
    }

    /// Shows the groups containing `text`, and offers to create `text` when it isn't a group yet.
    fn refilter(&self, win: &SupervisorWindow, text: &str) {
        let mut f = self.form.borrow_mut();
        let Some(ch) = f.chooser.as_mut() else { return };
        let needle = text.trim().to_uppercase();
        let all = list(&*self.ctx.products, ch.kind);
        let exact = all.iter().any(|g| g.name == needle);
        ch.shown = all.into_iter().filter(|g| g.name.contains(&needle)).collect();
        let items: Vec<SharedString> = ch.shown.iter().map(|g| SharedString::from(g.name.as_str())).collect();
        win.set_chooser_items(ModelRc::new(VecModel::from(items)));
        win.set_chooser_create_text(if needle.is_empty() || exact { String::new() } else { format!("+  Create “{needle}”") }.into());
        win.set_chooser_error("".into());
    }

    fn close_chooser(&self, win: &SupervisorWindow) {
        self.form.borrow_mut().chooser = None;
        win.set_chooser_title("".into());
    }

    /// Records the choice (`None` = no group). A group excludes the other kind.
    fn choose(&self, win: &SupervisorWindow, kind: GroupKind, group: Option<PriceGroup>) {
        {
            let mut f = self.form.borrow_mut();
            match kind {
                GroupKind::Alias => {
                    f.alias_id = group.as_ref().map(|g| g.id);
                    if group.is_some() {
                        f.variant_id = None;
                    }
                }
                GroupKind::Variant => {
                    f.variant_id = group.as_ref().map(|g| g.id);
                    if group.is_some() {
                        f.alias_id = None;
                    }
                }
            }
            // A priced group's numbers apply: show them, and keep them unless the cost is edited.
            f.baseline = None;
        }
        if let Some(g) = group.filter(|g| g.cost_cents > 0) {
            let markup_group = self.markup_group_id(win.get_form_group_index());
            win.set_form_cost(cost_text(g.cost_cents).into());
            self.form.borrow_mut().baseline = Some(Baseline {
                kind,
                group_name: g.name,
                cost_cents: g.cost_cents,
                price_cents: g.price_cents,
                markup_group,
            });
        }
        self.show_group_names(win);
        self.recalc(win);
        self.close_chooser(win);
    }

    fn pick(&self, win: &SupervisorWindow, index: i32) {
        let chosen = {
            let f = self.form.borrow();
            f.chooser
                .as_ref()
                .and_then(|c| usize::try_from(index).ok().and_then(|i| c.shown.get(i)).map(|g| (c.kind, g.clone())))
        };
        if let Some((kind, g)) = chosen {
            self.choose(win, kind, Some(g));
        }
    }

    fn create_group(&self, win: &SupervisorWindow) {
        let kind = match self.form.borrow().chooser.as_ref() {
            Some(c) => c.kind,
            None => return,
        };
        match self.ctx.products.create_price_group(kind, &win.get_chooser_search()) {
            Ok(g) => self.choose(win, kind, Some(g)),
            Err(e) => win.set_chooser_error(e.to_string().into()),
        }
    }

    /// Enter in the search box: take the only match, else create the typed name.
    fn submit_chooser(&self, win: &SupervisorWindow) {
        let only = self.form.borrow().chooser.as_ref().is_some_and(|c| c.shown.len() == 1);
        if only {
            self.pick(win, 0);
        } else if !win.get_chooser_create_text().is_empty() {
            self.create_group(win);
        }
    }

    // ── saving ──

    /// Validates the form, asks about anything risky (once), then stores it.
    fn save(&self, win: &SupervisorWindow, confirmed: bool) {
        let cost_cents = match parse_cost(&win.get_form_cost()) {
            Ok(c) => c,
            Err(e) => return win.set_form_error(e.to_string().into()),
        };
        let (alias_group_id, variant_group_id) = {
            let f = self.form.borrow();
            (f.alias_id, f.variant_id)
        };
        let discounts = match self.read_discounts(win) {
            Ok(d) => d,
            Err(msg) => return win.set_form_error(msg.into()),
        };
        let draft = ProductDraft {
            barcode: win.get_form_barcode().to_string(),
            name: win.get_form_name().to_string(),
            cost_cents,
            group_id: self.markup_group_id(win.get_form_group_index()),
            alias_group_id,
            variant_group_id,
            gct: win.get_form_gct(),
            discounts,
        };
        let mut write = match draft.into_write(self.group_markup(win.get_form_group_index())) {
            Ok(w) => w,
            Err(e) => return win.set_form_error(e.to_string().into()),
        };
        // Still exactly the group's numbers: keep the group's price rather than a recalculation.
        if self.at_baseline(win, cost_cents) {
            if let Some(b) = &self.form.borrow().baseline {
                write.price_cents = b.price_cents;
            }
        }

        let editing = self.form.borrow().editing.clone();
        if let (Some(original), false) = (&editing, confirmed) {
            if let Some(w) = warnings_for_edit(original, &draft).first() {
                return self.ask(win, Pending::BarcodeChange, "Change barcode?", &w.message(), "Change it", true);
            }
        }

        let result = match &editing {
            Some(p) => {
                let pct = self
                    .ctx
                    .config
                    .get("case_profit_pct")
                    .ok()
                    .flatten()
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .unwrap_or(0.10);
                self.ctx.products.update(p.id, &write, pct)
            }
            None => self.ctx.products.add(&write).map(|_| Default::default()),
        };
        match result {
            Ok(outcome) => {
                win.set_form_open(false);
                (self.reload)();
                let n = outcome.repriced_cases.len();
                if n > 0 {
                    let symbol = self.currency();
                    let plural = if n == 1 { "" } else { "s" };
                    let msg = match &outcome.group_sync {
                        Some(g) => format!(
                            "Cost ({}) and price ({}) synced to the group {}.\nLinked case product{plural} repriced: {}",
                            format_money(g.cost_cents, &symbol),
                            format_money(g.price_cents, &symbol),
                            g.group_name,
                            outcome.repriced_cases.join(", ")
                        ),
                        None => format!(
                            "Cost change cascaded to {n} linked case product{plural}.\n\
                             Case selling prices have been recalculated."
                        ),
                    };
                    self.ask(win, Pending::Notice, "Case Prices Updated", &msg, "OK", false);
                }
            }
            Err(e) => win.set_form_error(e.to_string().into()),
        }
    }

    fn ask_delete(&self, win: &SupervisorWindow, id: i64, close_form: bool) {
        match self.ctx.products.get(id) {
            Ok(Some(p)) => {
                self.ask(win, Pending::Delete { id, close_form }, "Delete", &format!("Delete '{}'?", p.name), "Delete", true)
            }
            Ok(None) => (self.reload)(),
            Err(e) => win.set_error_text(format!("Could not read the product: {e}").into()),
        }
    }
}

/// Connects the dialog and the row actions of the Products page to storage.
pub fn wire(win: &SupervisorWindow, ctx: Arc<Ctx>, reload: Rc<dyn Fn()>) {
    let env = Env {
        weak: win.as_weak(),
        ctx,
        form: Rc::new(RefCell::new(Form {
            editing: None,
            groups: Vec::new(),
            levels: Vec::new(),
            alias_id: None,
            variant_id: None,
            baseline: None,
            chooser: None,
            pending: Pending::None,
            symbol: "$".into(),
        })),
        reload,
    };

    let e = env.clone();
    win.on_add_product(move || e.with(|e, w| e.open(w, None)));
    let e = env.clone();
    win.on_edit_product(move |id| e.with(|e, w| e.open_edit(w, id.into())));
    let e = env.clone();
    win.on_delete_product(move |id| e.with(|e, w| e.ask_delete(w, id.into(), false)));

    let e = env.clone();
    win.on_form_changed(move || e.with(|e, w| e.recalc(w)));
    let e = env.clone();
    win.on_form_save(move || e.with(|e, w| e.save(w, false)));
    let e = env.clone();
    win.on_form_cancel(move || e.with(|_, w| w.set_form_open(false)));
    let e = env.clone();
    win.on_form_delete(move || {
        e.with(|e, w| {
            let id = e.form.borrow().editing.as_ref().map(|p| p.id);
            if let Some(id) = id {
                e.ask_delete(w, id, true);
            }
        })
    });

    let e = env.clone();
    win.on_form_discount_changed(move || e.with(|e, w| e.refresh_discount_summary(w)));
    let e = env.clone();
    win.on_form_pick_alias(move || e.with(|e, w| e.open_chooser(w, GroupKind::Alias)));
    let e = env.clone();
    win.on_form_pick_variant(move || e.with(|e, w| e.open_chooser(w, GroupKind::Variant)));
    let e = env.clone();
    win.on_chooser_searched(move |t| e.with(|e, w| e.refilter(w, &t)));
    let e = env.clone();
    win.on_chooser_submitted(move || e.with(|e, w| e.submit_chooser(w)));
    let e = env.clone();
    win.on_chooser_picked(move |i| e.with(|e, w| e.pick(w, i)));
    let e = env.clone();
    win.on_chooser_none(move || {
        e.with(|e, w| {
            let kind = e.form.borrow().chooser.as_ref().map(|c| c.kind);
            if let Some(kind) = kind {
                e.choose(w, kind, None);
            }
        })
    });
    let e = env.clone();
    win.on_chooser_create(move || e.with(|e, w| e.create_group(w)));
    let e = env.clone();
    win.on_chooser_cancelled(move || e.with(|e, w| e.close_chooser(w)));

    let e = env.clone();
    win.on_confirmed(move || {
        e.with(|e, w| match e.close_confirm(w) {
            Pending::Delete { id, close_form } => match e.ctx.products.delete(id) {
                Ok(_) => {
                    if close_form {
                        w.set_form_open(false);
                    }
                    (e.reload)();
                }
                Err(err) => {
                    let msg = format!("Delete Failed: {err}");
                    if close_form {
                        w.set_form_error(msg.into());
                    } else {
                        w.set_error_text(msg.into());
                    }
                }
            },
            Pending::BarcodeChange => e.save(w, true),
            Pending::Notice | Pending::None => {}
        })
    });
    let e = env;
    win.on_cancelled(move || {
        e.with(|e, w| {
            e.close_confirm(w);
        })
    });
}
