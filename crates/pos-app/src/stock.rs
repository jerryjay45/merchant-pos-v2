//! Inventory > Stock: what is left, adjusting it, and the history. The product list and the
//! variant-group pools work like the Products list: one page at a time, searched in the
//! database, loaded on a background thread (a late answer to an old request is dropped), so
//! opening the tab never loads everything. Rules live in `pos_core::stock`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use pos_core::{
    parse_qty, ConfigRepository, ListQuery, Pager, PoolRow, StockFilter, StockLevel, StockRepository, StockRow,
    UserRepository, DEFAULT_LOW_STOCK, REASONS,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::{supervisor::Ctx, HistoryRowData, PoolRowData, StockRowData, SupervisorWindow};

const PAGE_SIZE: u32 = 30;
const POOL_PAGE_SIZE: u32 = 20;
const HISTORY_ROWS: u32 = 200;

/// What the open adjust dialog is changing.
#[derive(Clone, Copy)]
enum Target {
    Product(i64),
    Pool(i64),
}

struct State {
    threshold: i64,
    search: String,
    filter: StockFilter,
    pager: Pager,
    token: u64,
    p_search: String,
    p_pager: Pager,
    p_token: u64,
    s_token: u64,
    target: Option<Target>,
}

impl State {
    fn new() -> Self {
        Self {
            threshold: DEFAULT_LOW_STOCK,
            search: String::new(),
            filter: StockFilter::All,
            pager: Pager::new(PAGE_SIZE),
            token: 0,
            p_search: String::new(),
            p_pager: Pager::new(POOL_PAGE_SIZE),
            p_token: 0,
            s_token: 0,
            target: None,
        }
    }
}

type Shared = Arc<Mutex<State>>;

#[derive(Clone)]
struct Env {
    weak: Weak<SupervisorWindow>,
    ctx: Arc<Ctx>,
    st: Shared,
    user_id: i64,
}

/// Handle the window keeps to (re)open the page.
#[derive(Clone)]
pub struct Page(Env);

impl Page {
    pub fn load(&self, win: &SupervisorWindow) {
        self.0.open(win);
    }
}

fn filter_from(index: i32) -> StockFilter {
    match index {
        1 => StockFilter::Low,
        2 => StockFilter::Out,
        _ => StockFilter::All,
    }
}

// ───────────────────────────── loading ─────────────────────────────

fn load_summary(win: &SupervisorWindow, ctx: &Arc<Ctx>, st: &Shared) {
    let (threshold, token) = {
        let mut s = st.lock().unwrap();
        s.s_token += 1;
        (s.threshold, s.s_token)
    };
    let (ctx, weak, st) = (ctx.clone(), win.as_weak(), st.clone());
    std::thread::spawn(move || {
        let result = ctx.products.stock_summary(threshold);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            if st.lock().unwrap().s_token != token {
                return;
            }
            win.set_st_alert(result.ok().and_then(|s| s.alert_text(threshold)).unwrap_or_default().into());
        });
    });
}

fn load_products(win: &SupervisorWindow, ctx: &Arc<Ctx>, st: &Shared) {
    let (query, filter, threshold, token) = {
        let mut s = st.lock().unwrap();
        s.token += 1;
        (ListQuery { search: s.search.clone(), limit: PAGE_SIZE, offset: s.pager.offset() }, s.filter, s.threshold, s.token)
    };
    win.set_st_loading(true);
    let (ctx, weak, st) = (ctx.clone(), win.as_weak(), st.clone());
    std::thread::spawn(move || {
        let result = ctx.products.stock_list(filter, threshold, &query);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if token != s.token {
                return;
            }
            win.set_st_loading(false);
            match result {
                Ok(page) => {
                    let before = s.pager.page;
                    s.pager.set_total(page.total);
                    if s.pager.page != before {
                        drop(s); // the list got shorter: fetch the last page that still exists
                        return load_products(&win, &ctx, &st);
                    }
                    show_products(&win, &s, &page.rows);
                }
                Err(e) => win.set_st_empty(format!("Could not read the stock: {e}").into()),
            }
        });
    });
}

fn show_products(win: &SupervisorWindow, s: &State, rows: &[StockRow]) {
    let data: Vec<StockRowData> = rows
        .iter()
        .map(|r| StockRowData {
            id: i32::try_from(r.id).unwrap_or(i32::MAX),
            name: r.name.as_str().into(),
            group: r.group.as_str().into(),
            stock: r.stock.to_string().into(),
            level: StockLevel::of(r.stock, s.threshold).code(),
            note: r.shared_with.as_ref().map(|g| format!("shared with {g}")).unwrap_or_default().into(),
        })
        .collect();
    let shown = data.len();
    win.set_st_rows(ModelRc::new(VecModel::from(data)));
    win.set_st_range(s.pager.range_label_of(shown, "product", "products").into());
    win.set_st_can_prev(s.pager.has_prev());
    win.set_st_can_next(s.pager.has_next());
    win.set_st_empty(
        if !s.search.trim().is_empty() {
            format!("No products match “{}”.", s.search.trim())
        } else {
            match s.filter {
                StockFilter::All => "No products yet.",
                StockFilter::Low => "Nothing is running low.",
                StockFilter::Out => "Nothing is out of stock.",
            }
            .to_string()
        }
        .into(),
    );
}

fn load_pools(win: &SupervisorWindow, ctx: &Arc<Ctx>, st: &Shared) {
    let (query, token) = {
        let mut s = st.lock().unwrap();
        s.p_token += 1;
        (ListQuery { search: s.p_search.clone(), limit: POOL_PAGE_SIZE, offset: s.p_pager.offset() }, s.p_token)
    };
    win.set_st_pool_loading(true);
    let (ctx, weak, st) = (ctx.clone(), win.as_weak(), st.clone());
    std::thread::spawn(move || {
        let result = ctx.products.pool_list(&query);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if token != s.p_token {
                return;
            }
            win.set_st_pool_loading(false);
            match result {
                Ok(page) => {
                    let before = s.p_pager.page;
                    s.p_pager.set_total(page.total);
                    if s.p_pager.page != before {
                        drop(s);
                        return load_pools(&win, &ctx, &st);
                    }
                    show_pools(&win, &s, &page.rows);
                }
                Err(e) => win.set_st_pool_empty(format!("Could not read the groups: {e}").into()),
            }
        });
    });
}

fn show_pools(win: &SupervisorWindow, s: &State, rows: &[PoolRow]) {
    let data: Vec<PoolRowData> = rows
        .iter()
        .map(|p| PoolRowData {
            id: i32::try_from(p.id).unwrap_or(i32::MAX),
            name: p.name.as_str().into(),
            stock: p.stock.to_string().into(),
            level: StockLevel::of(p.stock, s.threshold).code(),
        })
        .collect();
    let shown = data.len();
    win.set_st_pool_rows(ModelRc::new(VecModel::from(data)));
    win.set_st_pool_range(s.p_pager.range_label_of(shown, "group", "groups").into());
    win.set_st_pool_can_prev(s.p_pager.has_prev());
    win.set_st_pool_can_next(s.p_pager.has_next());
    win.set_st_pool_empty(
        if s.p_search.trim().is_empty() {
            "No variant groups yet. Create one from a product's Variant Group box.".to_string()
        } else {
            format!("No groups match “{}”.", s.p_search.trim())
        }
        .into(),
    );
}

// ───────────────────────────── the page ─────────────────────────────

impl Env {
    fn with(&self, f: impl FnOnce(&Env, &SupervisorWindow)) {
        if let Some(win) = self.weak.upgrade() {
            f(self, &win);
        }
    }

    /// Opening the page starts fresh and reads the low-stock limit the shop set.
    fn open(&self, win: &SupervisorWindow) {
        let threshold = self
            .ctx
            .config
            .get("low_stock_threshold")
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|t| *t >= 0)
            .unwrap_or(DEFAULT_LOW_STOCK);
        {
            let mut s = self.st.lock().unwrap();
            let tokens = (s.token, s.p_token, s.s_token); // keep counting up so older answers stay dropped
            *s = State::new();
            s.token = tokens.0;
            s.p_token = tokens.1;
            s.s_token = tokens.2;
            s.threshold = threshold;
        }
        win.set_st_search("".into());
        win.set_st_pool_search("".into());
        win.set_st_filter_index(0);
        win.set_adj_open(false);
        win.set_hist_open(false);
        self.reload_all(win);
    }

    fn reload_all(&self, win: &SupervisorWindow) {
        load_summary(win, &self.ctx, &self.st);
        load_products(win, &self.ctx, &self.st);
        load_pools(win, &self.ctx, &self.st);
    }

    fn search(&self, win: &SupervisorWindow, text: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.search = text.trim().to_string();
            s.pager.first();
        }
        load_products(win, &self.ctx, &self.st);
    }

    fn turn(&self, win: &SupervisorWindow, forward: bool) {
        {
            let mut s = self.st.lock().unwrap();
            if forward { s.pager.next() } else { s.pager.prev() }
        }
        load_products(win, &self.ctx, &self.st);
    }

    fn pool_search(&self, win: &SupervisorWindow, text: &str) {
        {
            let mut s = self.st.lock().unwrap();
            s.p_search = text.trim().to_string();
            s.p_pager.first();
        }
        load_pools(win, &self.ctx, &self.st);
    }

    fn pool_turn(&self, win: &SupervisorWindow, forward: bool) {
        {
            let mut s = self.st.lock().unwrap();
            if forward { s.p_pager.next() } else { s.p_pager.prev() }
        }
        load_pools(win, &self.ctx, &self.st);
    }

    // ── adjust dialog ──

    fn show_stock_in_dialog(&self, win: &SupervisorWindow, stock: i64) {
        let threshold = self.st.lock().unwrap().threshold;
        win.set_adj_current(stock.to_string().into());
        win.set_adj_level(StockLevel::of(stock, threshold).code());
    }

    fn open_adjust(&self, win: &SupervisorWindow, target: Target) {
        let (title, stock, note) = match target {
            Target::Product(id) => match self.ctx.products.stock_of(id) {
                Ok(Some(r)) => (
                    r.name,
                    r.stock,
                    r.shared_with.map(|g| format!("Shared with the variant group {g}: all its flavours use the same stock.")).unwrap_or_default(),
                ),
                Ok(None) => return self.reload_all(win), // it was deleted meanwhile
                Err(e) => return win.set_st_empty(format!("Could not open the product: {e}").into()),
            },
            Target::Pool(id) => match self.ctx.products.pool_of(id) {
                Ok(Some(p)) => (p.name, p.stock, "A variant group: all its flavours share this stock.".to_string()),
                Ok(None) => return self.reload_all(win),
                Err(e) => return win.set_st_pool_empty(format!("Could not open the group: {e}").into()),
            },
        };
        self.st.lock().unwrap().target = Some(target);
        win.set_adj_title(title.into());
        win.set_adj_note(note.into());
        win.set_adj_reasons(ModelRc::new(VecModel::from(REASONS.iter().map(|r| SharedString::from(*r)).collect::<Vec<_>>())));
        win.set_adj_reason_index(0);
        win.set_adj_qty("".into());
        win.set_adj_feedback("".into());
        self.show_stock_in_dialog(win, stock);
        win.set_adj_open(true);
    }

    fn feedback(&self, win: &SupervisorWindow, ok: bool, text: &str) {
        win.set_adj_feedback_ok(ok);
        win.set_adj_feedback(text.into());
    }

    fn adjust(&self, win: &SupervisorWindow, add: bool) {
        let Some(target) = self.st.lock().unwrap().target else { return };
        let qty = match parse_qty(&win.get_adj_qty()) {
            Ok(q) => i64::from(q),
            Err(e) => return self.feedback(win, false, &e),
        };
        let delta = if add { qty } else { -qty };
        let reason = REASONS.get(win.get_adj_reason_index().max(0) as usize).copied().unwrap_or("Other");
        let result = match target {
            Target::Product(id) => self.ctx.products.adjust_product(id, delta, reason, Some(self.user_id)),
            Target::Pool(id) => self.ctx.products.adjust_pool(id, delta, reason, Some(self.user_id)),
        };
        match result {
            Ok(change) => {
                self.show_stock_in_dialog(win, change.after);
                self.feedback(win, true, &change.message(delta));
                win.set_adj_qty("".into()); // so a second click can't apply it twice
                self.reload_all(win); // the lists behind the dialog follow
            }
            Err(e) => self.feedback(win, false, &e.to_string()),
        }
    }

    // ── history ──

    fn open_history(&self, win: &SupervisorWindow, target: Target) {
        let history = match target {
            Target::Product(id) => self.ctx.products.product_history(id, HISTORY_ROWS),
            Target::Pool(id) => self.ctx.products.pool_history(id, HISTORY_ROWS),
        };
        let h = match history {
            Ok(h) => h,
            Err(e) => return self.feedback(win, false, &format!("Could not read the history: {e}")),
        };
        let ids: Vec<i64> = h.rows.iter().filter_map(|r| r.by).collect();
        let names: HashMap<i64, String> = self.ctx.users.full_names(&ids).unwrap_or_default().into_iter().collect();
        let rows: Vec<HistoryRowData> = h
            .rows
            .iter()
            .map(|r| HistoryRowData {
                change: format!("{}{}", if r.change > 0 { "+" } else { "" }, r.change).into(),
                up: r.change > 0,
                reason: r.reason.as_str().into(),
                by: r.by.map_or("—".to_string(), |id| names.get(&id).cloned().unwrap_or_else(|| format!("user {id}"))).into(),
                at: r.at.as_str().into(),
            })
            .collect();
        win.set_hist_title(h.title.into());
        win.set_hist_empty("No stock changes recorded yet.".into());
        win.set_hist_rows(ModelRc::new(VecModel::from(rows)));
        win.set_hist_open(true);
    }
}

/// Connects the page and its dialogs to storage and returns the handle the window uses to open it.
pub fn wire(win: &SupervisorWindow, ctx: Arc<Ctx>, user_id: i64) -> Page {
    let env = Env { weak: win.as_weak(), ctx, st: Arc::new(Mutex::new(State::new())), user_id };

    let e = env.clone();
    win.on_st_search_submitted(move || e.with(|e, w| e.search(w, &w.get_st_search())));
    let e = env.clone();
    win.on_st_search_cleared(move || {
        e.with(|e, w| {
            w.set_st_search("".into());
            e.search(w, "");
        })
    });
    let e = env.clone();
    win.on_st_refresh(move || e.with(|e, w| e.reload_all(w)));
    let e = env.clone();
    win.on_st_filter_changed(move || {
        e.with(|e, w| {
            {
                let mut s = e.st.lock().unwrap();
                s.filter = filter_from(w.get_st_filter_index());
                s.pager.first();
            }
            load_products(w, &e.ctx, &e.st);
        })
    });
    let e = env.clone();
    win.on_st_prev(move || e.with(|e, w| e.turn(w, false)));
    let e = env.clone();
    win.on_st_next(move || e.with(|e, w| e.turn(w, true)));
    let e = env.clone();
    win.on_st_adjust(move |id| e.with(|e, w| e.open_adjust(w, Target::Product(id.into()))));
    let e = env.clone();
    win.on_st_history(move |id| e.with(|e, w| e.open_history(w, Target::Product(id.into()))));

    let e = env.clone();
    win.on_st_pool_search_submitted(move || e.with(|e, w| e.pool_search(w, &w.get_st_pool_search())));
    let e = env.clone();
    win.on_st_pool_search_cleared(move || {
        e.with(|e, w| {
            w.set_st_pool_search("".into());
            e.pool_search(w, "");
        })
    });
    let e = env.clone();
    win.on_st_pool_prev(move || e.with(|e, w| e.pool_turn(w, false)));
    let e = env.clone();
    win.on_st_pool_next(move || e.with(|e, w| e.pool_turn(w, true)));
    let e = env.clone();
    win.on_st_pool_adjust(move |id| e.with(|e, w| e.open_adjust(w, Target::Pool(id.into()))));
    let e = env.clone();
    win.on_st_pool_history(move |id| e.with(|e, w| e.open_history(w, Target::Pool(id.into()))));

    let e = env.clone();
    win.on_adj_add(move || e.with(|e, w| e.adjust(w, true)));
    let e = env.clone();
    win.on_adj_remove(move || e.with(|e, w| e.adjust(w, false)));
    let e = env.clone();
    win.on_adj_history(move || {
        e.with(|e, w| {
            let target = e.st.lock().unwrap().target;
            if let Some(t) = target {
                e.open_history(w, t);
            }
        })
    });
    let e = env.clone();
    win.on_adj_close(move || {
        e.with(|e, w| {
            e.st.lock().unwrap().target = None;
            w.set_adj_open(false);
        })
    });
    win.on_hist_close({
        let weak = win.as_weak();
        move || {
            if let Some(w) = weak.upgrade() {
                w.set_hist_open(false);
            }
        }
    });

    Page(env)
}
