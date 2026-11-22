//! The Add / Edit Product dialog, the alias / variant group chooser and the Delete confirmation
//! on the Products page. The rules (price from markup, required fields, the barcode warning)
//! live in `pos_core::product_edit`; this file only moves them between the window and the
//! database.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use pos_core::{
    fmt_percent, format_money, parse_cost, parse_date, price_case, price_from_markup, price_hint, savings_cents,
    warnings_for_edit, CaseLink, CaseSetting, ConfigRepository, DiscountLevel, DiscountSetting, Group, GroupKind, Membership,
    PriceGroup, PriceMode, ProductDetail, ProductDraft, ProductQuery, ProductRepository, SlotSetting, SlotSource, Unit,
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

/// The product or variant group picker for "this case is made of ...".
struct CaseChooser {
    /// true = single products, false = variant groups.
    products: bool,
    shown: Vec<(i64, String)>,
}

struct Form {
    /// What the case is made of, and how it is shown on the picker button.
    case_parent: Option<(CaseLink, String)>,
    case_chooser: Option<CaseChooser>,
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
        if win.get_form_case() && win.get_form_case_link_index() != 0 {
            return self.recalc_case(win);
        }
        win.set_form_cost_locked(false);
        win.set_form_case_hint("".into());
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
        let (case, price) = editing.as_ref().map(|p| (p.case.clone(), p.price_cents)).unwrap_or((None, 0));
        self.fill_case(win, &case, price);

        let (title, notice, barcode, name, cost, gct, group_index) = match &editing {
            Some(p) => (
                "Edit Product",
                String::new(),
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

    // ── case / pack items ──

    fn case_profit_pct(&self) -> f64 {
        self.ctx
            .config
            .get("case_profit_pct")
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.10)
    }

    /// The cost and price of the single (or variant group) the case is made of.
    fn case_unit(&self) -> Option<Unit> {
        let link = self.form.borrow().case_parent.as_ref().map(|(l, _)| *l)?;
        match link {
            CaseLink::Product(id) => {
                self.ctx.products.get(id).ok().flatten().map(|p| Unit { cost_cents: p.cost_cents, price_cents: p.price_cents })
            }
            CaseLink::VariantGroup(id) => list(&*self.ctx.products, GroupKind::Variant)
                .into_iter()
                .find(|g| g.id == id)
                .map(|g| Unit { cost_cents: g.cost_cents, price_cents: g.price_cents }),
        }
    }

    fn read_case_mode(&self, win: &SupervisorWindow) -> Result<PriceMode, String> {
        match win.get_form_case_mode_index() {
            1 => win
                .get_form_case_discount()
                .trim()
                .trim_end_matches('%')
                .replace(',', ".")
                .parse::<f64>()
                .map(|percent| PriceMode::PackDiscount { percent })
                .map_err(|_| "Enter the pack discount as a number, like 10 or 7.5.".to_string()),
            2 => Ok(PriceMode::Manual),
            _ => Ok(PriceMode::Markup),
        }
    }

    fn read_case(&self, win: &SupervisorWindow) -> Result<Option<CaseSetting>, String> {
        if !win.get_form_case() {
            return Ok(None);
        }
        let link = match win.get_form_case_link_index() {
            0 => None,
            n => {
                let parent = self.form.borrow().case_parent.as_ref().map(|(l, _)| *l);
                Some(parent.ok_or_else(|| {
                    if n == 1 { "Choose the single product this case is made of." } else { "Choose the variant group this case is made of." }
                        .to_string()
                })?)
            }
        };
        let units = win
            .get_form_case_units()
            .trim()
            .parse::<u32>()
            .map_err(|_| "Enter how many units are in the case, like 12.".to_string())?;
        let mode = if link.is_some() { self.read_case_mode(win)? } else { PriceMode::Markup };
        Ok(Some(CaseSetting { link, units, mode }))
    }

    /// Live cost, price and explanation for a case linked to a single product or variant group.
    fn recalc_case(&self, win: &SupervisorWindow) {
        let symbol = self.form.borrow().symbol.clone();
        let unit = self.case_unit();
        let units = win.get_form_case_units().trim().parse::<u32>().ok().filter(|u| *u >= 1);
        let mode = self.read_case_mode(win).ok();
        let (Some(unit), Some(units), Some(mode)) = (unit, units, mode) else {
            win.set_form_cost_locked(false);
            win.set_form_price_text("".into());
            win.set_form_price_hint("".into());
            let hint = if self.form.borrow().case_parent.is_none() {
                "Choose the single product (or variant group) this case is made of."
            } else if units.is_none() {
                "Enter how many units are in the case."
            } else {
                ""
            };
            win.set_form_case_hint(hint.into());
            return;
        };
        let typed_cost = parse_cost(&win.get_form_cost()).unwrap_or(0);
        let typed_price = parse_cost(&win.get_form_case_price()).unwrap_or(0);
        let pct = self.case_profit_pct();
        let (cost, price) = price_case(typed_cost, typed_price, unit, units, mode, pct);
        let locked = unit.cost_cents > 0;
        if locked {
            win.set_form_cost(cost_text(cost).into());
        }
        win.set_form_cost_locked(locked);
        win.set_form_price_text(format_money(price, &symbol).into());
        win.set_form_price_hint("".into());

        let m = |c: i64| format_money(c, &symbol);
        let singles = unit.price_cents * i64::from(units);
        let mut hint = if !locked && !matches!(mode, PriceMode::PackDiscount { .. }) {
            "The single has no cost yet, so the cost typed above is used.".to_string()
        } else {
            match mode {
                PriceMode::Markup => format!(
                    "{units} × {} = {}, plus {}% shop profit = {}.",
                    m(unit.cost_cents),
                    m(cost),
                    fmt_percent(pct * 100.0),
                    m(price)
                ),
                PriceMode::PackDiscount { percent } => format!(
                    "{units} × {} = {}, {}% off = {} (saves {}). Cost is {units} × {} = {}.",
                    m(unit.price_cents),
                    m(singles),
                    fmt_percent(percent),
                    m(price),
                    m(savings_cents(unit.price_cents, units, price)),
                    m(unit.cost_cents),
                    m(cost)
                ),
                PriceMode::Manual => format!(
                    "Cost is {units} × {} = {}. The price is yours and stays put when the single changes; {units} singles sell for {}.",
                    m(unit.cost_cents),
                    m(cost),
                    m(singles)
                ),
            }
        };
        if price < cost {
            hint.push_str("  Warning: this case would sell below its cost.");
        }
        win.set_form_case_hint(hint.into());
    }

    fn show_case_parent(&self, win: &SupervisorWindow) {
        let name = self.form.borrow().case_parent.as_ref().map(|(_, n)| n.clone()).unwrap_or_default();
        win.set_form_case_link_name(name.into());
    }

    /// The case checkbox, "Made of", units or price mode changed.
    fn case_changed(&self, win: &SupervisorWindow) {
        if win.get_form_case() {
            // a case is neither in an alias group nor in a variant group
            {
                let mut f = self.form.borrow_mut();
                f.alias_id = None;
                f.variant_id = None;
                f.baseline = None;
            }
            self.show_group_names(win);
            // the chosen single / group has to match "Made of"
            let keep = {
                let f = self.form.borrow();
                matches!(
                    (&f.case_parent, win.get_form_case_link_index()),
                    (Some((CaseLink::Product(_), _)), 1) | (Some((CaseLink::VariantGroup(_), _)), 2)
                )
            };
            if !keep {
                self.form.borrow_mut().case_parent = None;
            }
        }
        self.show_case_parent(win);
        self.recalc(win);
    }

    fn fill_case(&self, win: &SupervisorWindow, case: &Option<CaseSetting>, price_cents: i64) {
        let (on, link_index, units, mode_index, discount, parent) = match case {
            None => (false, 1, String::new(), 0, String::new(), None),
            Some(c) => {
                let (link_index, parent) = match c.link {
                    None => (0, None),
                    Some(CaseLink::Product(id)) => (
                        1,
                        self.ctx
                            .products
                            .get(id)
                            .ok()
                            .flatten()
                            .map(|p| (CaseLink::Product(id), format!("{}  ·  {}", p.name, p.barcode))),
                    ),
                    Some(CaseLink::VariantGroup(id)) => (
                        2,
                        list(&*self.ctx.products, GroupKind::Variant)
                            .into_iter()
                            .find(|g| g.id == id)
                            .map(|g| (CaseLink::VariantGroup(id), g.name)),
                    ),
                };
                let (mode_index, discount) = match c.mode {
                    PriceMode::Markup => (0, String::new()),
                    PriceMode::PackDiscount { percent } => (1, fmt_percent(percent)),
                    PriceMode::Manual => (2, String::new()),
                };
                (true, link_index, c.units.to_string(), mode_index, discount, parent)
            }
        };
        self.form.borrow_mut().case_parent = parent;
        win.set_form_case(on);
        win.set_form_case_link_index(link_index);
        win.set_form_case_units(units.into());
        win.set_form_case_mode_index(mode_index);
        win.set_form_case_discount(discount.into());
        win.set_form_case_price(if mode_index == 2 { cost_text(price_cents) } else { String::new() }.into());
        self.show_case_parent(win);
    }

    fn open_case_chooser(&self, win: &SupervisorWindow) {
        let products = win.get_form_case_link_index() == 1;
        self.form.borrow_mut().case_chooser = Some(CaseChooser { products, shown: Vec::new() });
        win.set_chooser_search("".into());
        win.set_chooser_error("".into());
        win.set_chooser_create_text("".into());
        let current = self.form.borrow().case_parent.as_ref().map(|(_, n)| n.clone()).unwrap_or_default();
        win.set_chooser_current(current.into());
        self.refilter_case(win, "");
        win.set_chooser_title(if products { "Select the single product" } else { "Select the variant group" }.into());
    }

    fn refilter_case(&self, win: &SupervisorWindow, text: &str) {
        let products = match self.form.borrow().case_chooser.as_ref() {
            Some(c) => c.products,
            None => return,
        };
        let own = self.form.borrow().editing.as_ref().map(|p| p.id);
        let needle = text.trim().to_string();
        let entries: Vec<(i64, String)> = if products {
            // a case can't be made of itself or of another case
            match self.ctx.products.list(&ProductQuery { search: needle, limit: 40, offset: 0 }) {
                Ok(page) => page
                    .rows
                    .into_iter()
                    .filter(|r| r.membership != Membership::Case && Some(r.id) != own)
                    .map(|r| (r.id, format!("{}  ·  {}", r.name, r.barcode)))
                    .collect(),
                Err(e) => {
                    win.set_chooser_error(format!("Could not search: {e}").into());
                    Vec::new()
                }
            }
        } else {
            let n = needle.to_uppercase();
            list(&*self.ctx.products, GroupKind::Variant)
                .into_iter()
                .filter(|g| g.name.contains(&n))
                .map(|g| (g.id, g.name))
                .collect()
        };
        let items: Vec<SharedString> = entries.iter().map(|(_, n)| SharedString::from(n.as_str())).collect();
        if let Some(c) = self.form.borrow_mut().case_chooser.as_mut() {
            c.shown = entries;
        }
        win.set_chooser_items(ModelRc::new(VecModel::from(items)));
    }

    fn pick_case(&self, win: &SupervisorWindow, index: i32) {
        let chosen = {
            let f = self.form.borrow();
            f.case_chooser.as_ref().and_then(|c| {
                usize::try_from(index).ok().and_then(|i| c.shown.get(i)).map(|(id, label)| {
                    (if c.products { CaseLink::Product(*id) } else { CaseLink::VariantGroup(*id) }, label.clone())
                })
            })
        };
        if let Some(parent) = chosen {
            self.form.borrow_mut().case_parent = Some(parent);
            self.close_chooser(win);
            self.show_case_parent(win);
            self.recalc(win);
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
        if self.form.borrow().case_chooser.is_some() {
            return self.refilter_case(win, text);
        }
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
        {
            let mut f = self.form.borrow_mut();
            f.chooser = None;
            f.case_chooser = None;
        }
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
        if self.form.borrow().case_chooser.is_some() {
            return self.pick_case(win, index);
        }
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
        let only = {
            let f = self.form.borrow();
            f.chooser.as_ref().map(|c| c.shown.len()).or(f.case_chooser.as_ref().map(|c| c.shown.len())) == Some(1)
        };
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
        let case = match self.read_case(win) {
            Ok(c) => c,
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
            case,
        };
        let mut write = match draft.into_write(self.group_markup(win.get_form_group_index())) {
            Ok(w) => w,
            Err(e) => return win.set_form_error(e.to_string().into()),
        };
        // A case with a typed price keeps exactly that price (its cost still follows the single).
        if let Some(CaseSetting { link: Some(_), mode: PriceMode::Manual, .. }) = &write.case {
            match parse_cost(&win.get_form_case_price()) {
                Ok(p) => write.price_cents = p,
                Err(e) => return win.set_form_error(e.to_string().into()),
            }
        }
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

        let pct = self.case_profit_pct();
        let result = match &editing {
            Some(p) => self.ctx.products.update(p.id, &write, pct),
            None => self.ctx.products.add(&write, pct).map(|_| Default::default()),
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
            case_parent: None,
            case_chooser: None,
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
    win.on_form_case_changed(move || e.with(|e, w| e.case_changed(w)));
    let e = env.clone();
    win.on_form_pick_case_parent(move || e.with(|e, w| e.open_case_chooser(w)));
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
            if e.form.borrow().case_chooser.is_some() {
                e.form.borrow_mut().case_parent = None;
                e.close_chooser(w);
                e.show_case_parent(w);
                e.recalc(w);
                return;
            }
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
