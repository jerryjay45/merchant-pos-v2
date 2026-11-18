//! The supervisor / manager window. All decisions (which areas a role sees, paging, money format,
//! what a search matches) live in `pos-core` / `pos-db`; this file only moves data between them
//! and the window.

use std::sync::{Arc, Mutex};

use pos_core::{
    areas_for, format_money, nav, ConfigRepository, Pager, ProductQuery, ProductRepository, ProductRow, Role, User,
};
use pos_db::{ConfigDb, ProductsDb};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{NavItem, ProductRowData, SupervisorWindow};

const PAGE_SIZE: u32 = 50; // same as the Python list

/// Everything a signed-in window needs from storage.
pub struct Ctx {
    pub config: Arc<ConfigDb>,
    pub products: Arc<ProductsDb>,
}

/// Search text, current page, and a counter so a slow, older load can't overwrite a newer one.
struct ListState {
    search: String,
    pager: Pager,
    token: u64,
    currency: String,
}

type Shared = Arc<Mutex<ListState>>;

fn nav_model(items: impl IntoIterator<Item = (&'static str, &'static str)>) -> ModelRc<NavItem> {
    ModelRc::new(VecModel::from(
        items.into_iter().map(|(k, t)| NavItem { key: k.into(), title: t.into() }).collect::<Vec<_>>(),
    ))
}

fn row_data(r: &ProductRow, currency: &str) -> ProductRowData {
    let tags = r.tags();
    ProductRowData {
        name: r.name.as_str().into(),
        barcode: r.barcode.as_str().into(),
        cost: format_money(r.cost_cents, currency).into(),
        price: format_money(r.price_cents, currency).into(),
        tags: if tags.is_empty() { "—".into() } else { tags.join("  ·  ").into() },
    }
}

/// Loads the current page off the UI thread. Only the newest request may update the window.
fn load(win: &SupervisorWindow, ctx: &Arc<Ctx>, state: &Shared) {
    let (query, token) = {
        let mut s = state.lock().unwrap();
        s.token += 1;
        (
            ProductQuery { search: s.search.clone(), limit: PAGE_SIZE, offset: s.pager.offset() },
            s.token,
        )
    };
    win.set_loading(true);
    win.set_error_text("".into());

    let (ctx, weak, state) = (ctx.clone(), win.as_weak(), state.clone());
    std::thread::spawn(move || {
        let result = ctx.products.list(&query);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(win) = weak.upgrade() else { return };
            let mut s = state.lock().unwrap();
            if token != s.token {
                return; // a newer request replaced this one
            }
            win.set_loading(false);
            match result {
                Ok(page) => {
                    let before = s.pager.page;
                    s.pager.set_total(page.total);
                    if s.pager.page != before {
                        // The list got shorter (e.g. a narrower search): the page we asked for no
                        // longer exists, so fetch the last one that does.
                        drop(s);
                        return load(&win, &ctx, &state);
                    }
                    let rows: Vec<_> = page.rows.iter().map(|r| row_data(r, &s.currency)).collect();
                    let shown = rows.len();
                    win.set_rows(ModelRc::new(VecModel::from(rows)));
                    win.set_page_label(s.pager.page_label().into());
                    win.set_range_label(s.pager.range_label(shown).into());
                    win.set_can_prev(s.pager.has_prev());
                    win.set_can_next(s.pager.has_next());
                    win.set_empty_text(if s.search.trim().is_empty() {
                        "No products yet. Add one, or import your list, to get started.".into()
                    } else {
                        SharedString::from(format!("No products match “{}”.", s.search.trim()))
                    });
                }
                Err(e) => {
                    win.set_rows(ModelRc::new(VecModel::from(Vec::<ProductRowData>::new())));
                    win.set_range_label("".into());
                    win.set_error_text(format!("Could not read the product list: {e}").into());
                }
            }
        });
    });
}

fn show_page(win: &SupervisorWindow, key: &str, title: &str, ctx: &Arc<Ctx>, state: &Shared, first_time: &mut bool) {
    win.set_current_page(key.into());
    win.set_page_title(title.into());
    let built = nav::is_built(key);
    win.set_page_built(built);
    if key == "products" && built && *first_time {
        *first_time = false; // pages load the first time they are opened, like the Python tabs
        load(win, ctx, state);
    }
}

/// Builds the window for a signed-in supervisor or manager. `on_logout` runs when Logout is pressed.
pub fn open(user: &User, ctx: Arc<Ctx>, on_logout: impl Fn() + 'static) -> Result<SupervisorWindow, slint::PlatformError> {
    let win = SupervisorWindow::new()?;
    let currency = ctx.config.get("currency_symbol").ok().flatten().filter(|s| !s.is_empty()).unwrap_or_else(|| "$".into());
    let state: Shared = Arc::new(Mutex::new(ListState {
        search: String::new(),
        pager: Pager::new(PAGE_SIZE),
        token: 0,
        currency,
    }));

    let role_text = match user.role {
        Role::Manager => "Manager",
        _ => "Supervisor",
    };
    win.set_role_text(role_text.into());
    win.set_user_name(user.full_name.to_uppercase().into());

    let areas = areas_for(user.role);
    win.set_areas(nav_model(areas.iter().map(|a| (a.key, a.title))));

    let areas = Arc::new(areas);
    let first_load = std::rc::Rc::new(std::cell::Cell::new(true));

    // Open the first area / first page.
    let select_area = {
        let (weak, areas, ctx, state, first_load) = (win.as_weak(), areas.clone(), ctx.clone(), state.clone(), first_load.clone());
        move |key: &str, page_key: Option<&str>| {
            let Some(win) = weak.upgrade() else { return };
            let Some(area) = areas.iter().find(|a| a.key == key) else { return };
            win.set_current_area(area.key.into());
            win.set_pages(nav_model(area.pages.iter().map(|p| (p.key, p.title))));
            let page = page_key
                .and_then(|k| area.pages.iter().find(|p| p.key == k))
                .or_else(|| area.pages.first());
            if let Some(p) = page {
                let mut first = first_load.get();
                show_page(&win, p.key, p.title, &ctx, &state, &mut first);
                first_load.set(first);
            }
        }
    };
    let select_area = std::rc::Rc::new(select_area);

    // The Python window opens on Sales; until the Sales pages exist here, start on Inventory > Products.
    let start = areas.iter().find(|a| a.pages.iter().any(|p| nav::is_built(p.key))).or_else(|| areas.first());
    if let Some(a) = start {
        let built = a.pages.iter().find(|p| nav::is_built(p.key)).map(|p| p.key);
        select_area(a.key, built);
    }

    let sa = select_area.clone();
    win.on_area_selected(move |k| sa(k.as_str(), None));

    let (weak, areas2, ctx2, state2, first2) = (win.as_weak(), areas.clone(), ctx.clone(), state.clone(), first_load.clone());
    win.on_page_selected(move |k| {
        let Some(win) = weak.upgrade() else { return };
        let area_key = win.get_current_area();
        let Some(p) = areas2.iter().find(|a| a.key == area_key.as_str()).and_then(|a| a.pages.iter().find(|p| p.key == k.as_str())) else { return };
        let mut first = first2.get();
        show_page(&win, p.key, p.title, &ctx2, &state2, &mut first);
        first2.set(first);
    });

    // search / paging
    let (weak, c, s) = (win.as_weak(), ctx.clone(), state.clone());
    win.on_search(move |text| {
        let Some(win) = weak.upgrade() else { return };
        {
            let mut st = s.lock().unwrap();
            st.search = text.trim().to_string();
            st.pager.first();
        }
        load(&win, &c, &s);
    });
    let (weak, c, s) = (win.as_weak(), ctx.clone(), state.clone());
    win.on_clear_search(move || {
        let Some(win) = weak.upgrade() else { return };
        {
            let mut st = s.lock().unwrap();
            st.search.clear();
            st.pager.first();
        }
        win.set_search_text("".into());
        load(&win, &c, &s);
    });
    let (weak, c, s) = (win.as_weak(), ctx.clone(), state.clone());
    win.on_refresh(move || {
        let Some(win) = weak.upgrade() else { return };
        s.lock().unwrap().pager.first();
        load(&win, &c, &s);
    });
    let (weak, c, s) = (win.as_weak(), ctx.clone(), state.clone());
    win.on_prev_page(move || {
        let Some(win) = weak.upgrade() else { return };
        s.lock().unwrap().pager.prev();
        load(&win, &c, &s);
    });
    let (weak, c, s) = (win.as_weak(), ctx, state);
    win.on_next_page(move || {
        let Some(win) = weak.upgrade() else { return };
        s.lock().unwrap().pager.next();
        load(&win, &c, &s);
    });

    win.on_logout(on_logout);
    Ok(win)
}
