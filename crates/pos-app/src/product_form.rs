//! The Add / Edit Product dialog and the Delete confirmation on the Products page. The rules
//! (price from markup, required fields, the barcode warning) live in `pos_core::product_edit`;
//! this file only moves them between the window and the database.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use pos_core::{
    format_money, parse_cost, price_from_markup, price_hint, warnings_for_edit, ConfigRepository, Group, Membership,
    ProductDetail, ProductDraft, ProductError, ProductRepository,
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

struct Form {
    /// The product being edited (`None` = adding).
    editing: Option<ProductDetail>,
    groups: Vec<Group>,
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

fn not_editable_notice(m: &Membership) -> String {
    let what = match m {
        Membership::Alias(n) => format!("in the alias group “{n}”"),
        Membership::Variant(n) => format!("in the variant group “{n}”"),
        Membership::Case => "a case item".to_string(),
        Membership::Standalone => return String::new(),
    };
    format!(
        "This product is {what}. Its price comes from that group or its linked product, and editing \
         those isn't available in the Rust version yet. Use the Python app for now."
    )
}

impl Env {
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

    fn group_id(&self, index: i32) -> Option<i64> {
        let f = self.form.borrow();
        usize::try_from(index - 1).ok().and_then(|i| f.groups.get(i)).map(|g| g.id)
    }

    /// Live selling price under the cost box.
    fn recalc(&self, win: &SupervisorWindow) {
        let symbol = self.form.borrow().symbol.clone();
        match parse_cost(&win.get_form_cost()) {
            Ok(cost) => {
                let markup = self.group_markup(win.get_form_group_index()).unwrap_or(0.0);
                win.set_form_price_text(format_money(price_from_markup(cost, markup), &symbol).into());
                win.set_form_price_hint(price_hint(cost, markup, &symbol).into());
            }
            Err(_) => {
                win.set_form_price_text("".into());
                win.set_form_price_hint("".into());
            }
        }
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
        {
            let mut f = self.form.borrow_mut();
            f.editing = editing;
            f.groups = groups;
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
        self.recalc(win);
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

    /// Validates the form, asks about anything risky (once), then stores it.
    fn save(&self, win: &SupervisorWindow, confirmed: bool) {
        let draft = match parse_cost(&win.get_form_cost()) {
            Ok(cost_cents) => ProductDraft {
                barcode: win.get_form_barcode().to_string(),
                name: win.get_form_name().to_string(),
                cost_cents,
                group_id: self.group_id(win.get_form_group_index()),
                gct: win.get_form_gct(),
            },
            Err(e) => return win.set_form_error(e.to_string().into()),
        };
        let write = match draft.into_write(self.group_markup(win.get_form_group_index())) {
            Ok(w) => w,
            Err(e) => return win.set_form_error(e.to_string().into()),
        };

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
                self.ctx.products.update(p.id, &write, pct).map(|o| o.cases_updated)
            }
            None => self.ctx.products.add(&write).map(|_| 0),
        };
        match result {
            Ok(cases_updated) => {
                win.set_form_open(false);
                (self.reload)();
                if cases_updated > 0 {
                    let plural = if cases_updated == 1 { "" } else { "s" };
                    self.ask(
                        win,
                        Pending::Notice,
                        "Case Prices Updated",
                        &format!(
                            "Cost change cascaded to {cases_updated} linked case product{plural}.\n\
                             Case selling prices have been recalculated."
                        ),
                        "OK",
                        false,
                    );
                }
            }
            Err(ProductError::DuplicateBarcode(b)) => {
                win.set_form_error(ProductError::DuplicateBarcode(b).to_string().into())
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
        form: Rc::new(RefCell::new(Form { editing: None, groups: Vec::new(), pending: Pending::None, symbol: "$".into() })),
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
