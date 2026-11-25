use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use pos_core::{
    check_session_gate, needs_setup, new_password_hash, run_setup,
    setup::{parse_gct_rate, validate_business, validate_manager, validate_terminal_id},
    AuthError, AuthService, BusinessInfo, ConfigRepository, Role, SetupData, SetupError, User,
    UserRepository,
};
use pos_db::{ConfigDb, ProductsDb, UsersDb};
use slint::ComponentHandle;

mod groups;
mod product_form;
mod supervisor;

slint::include_modules!();

const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Same layout as the Python app: `<home>/data/{users,config,products,checkout}.db`.
/// `MERCHANTPOS_HOME` wins (that's what the Flatpak sets), so you can also point this at an
/// existing Python data folder to test against real data.
fn data_dir() -> PathBuf {
    let home = if let Some(h) = std::env::var_os("MERCHANTPOS_HOME") {
        PathBuf::from(h)
    } else if cfg!(windows) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\MerchantPOS"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("merchantpos")
    };
    home.join("data")
}

/// Business-name font size: shrink from 20px toward 14px until the longest word fits the card.
/// (Estimate; the Python version measures real glyphs.)
fn fit_title_px(name: &str, max_width: f32) -> i32 {
    let longest = name
        .lines()
        .flat_map(str::split_whitespace)
        .map(|w| w.chars().count())
        .max()
        .unwrap_or(0) as f32;
    let mut px = 20;
    while px > 14 && longest * px as f32 * 0.62 > max_width {
        px -= 1;
    }
    px
}

/// Dev helper for adding extra users from a terminal (the first manager is created by the
/// setup wizard):
///   pos-app add-user <username> <cashier|supervisor|manager> [full name...]
fn add_user(users: &UsersDb, args: &[String]) -> Result<(), String> {
    let usage = "usage: add-user <username> <cashier|supervisor|manager> [full name]";
    let username = args.first().ok_or(usage)?;
    let role: Role = args.get(1).ok_or(usage)?.parse()?;
    let full_name = if args.len() > 2 { args[2..].join(" ") } else { username.clone() };

    print!("Password for {username}: ");
    io::stdout().flush().ok();
    let mut pw = String::new();
    io::stdin().read_line(&mut pw).map_err(|e| e.to_string())?;
    if pw.trim().is_empty() {
        return Err("password can't be empty".into());
    }
    let hash = new_password_hash(&pw).map_err(|e| e.to_string())?;
    users.insert_user(&full_name, username, &hash, role).map_err(|e| e.to_string())?;
    println!("Created {role} '{}'.", username.to_uppercase());
    Ok(())
}

const LAST_PAGE: i32 = 6;

/// Reads the wizard's fields into a `SetupData`.
fn collect_setup(win: &SetupWindow) -> SetupData {
    let gct_enabled = win.get_gct_enabled();
    SetupData {
        business: BusinessInfo {
            name: win.get_biz_name().to_string(),
            address: win.get_biz_address().to_string(),
            phone: win.get_biz_phone().to_string(),
            email: win.get_biz_email().to_string(),
            tax_id: win.get_biz_tax_id().to_string(),
            receipt_footer: win.get_biz_footer().to_string(),
        },
        terminal_id: win.get_terminal_id().to_string(),
        currency_symbol: win.get_currency().to_string(),
        gct_enabled,
        // A disabled GCT keeps the default rate if the box was left blank.
        gct_rate_pct: parse_gct_rate(&win.get_gct_rate()).unwrap_or(16.5),
        manager_full_name: win.get_mgr_name().to_string(),
        manager_username: win.get_mgr_user().to_string(),
        manager_password: win.get_mgr_pass().to_string(),
        receipt_printer_name: win.get_printer_name().to_string(),
        receipt_copies: win.get_copies().clamp(1, 5) as u8,
    }
}

/// Checks the page the user is leaving; the message is shown under the page.
fn validate_setup_page(win: &SetupWindow, page: i32) -> Result<(), SetupError> {
    match page {
        1 => validate_business(&win.get_biz_name()),
        2 => validate_terminal_id(&win.get_terminal_id()),
        3 if win.get_gct_enabled() => parse_gct_rate(&win.get_gct_rate()).map(|_| ()),
        4 => validate_manager(
            &win.get_mgr_name(),
            &win.get_mgr_user(),
            &win.get_mgr_pass(),
            &win.get_mgr_confirm(),
        ),
        _ => Ok(()),
    }
}

fn setup_summary(win: &SetupWindow) -> String {
    let d = collect_setup(win);
    let first_line = |s: &str| s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("-").to_uppercase();
    let mut lines = vec![
        format!("✓  Business: {}", first_line(&d.business.name)),
        format!("✓  Terminal ID: {}", d.terminal_id.trim().to_uppercase()),
        format!("✓  Manager account: {}", d.manager_username.trim().to_uppercase()),
    ];
    if d.receipt_printer_name.trim().is_empty() {
        lines.push("-  Printer: not configured (set in Manager > Settings)".into());
    } else {
        lines.push(format!("✓  Printer: {}", d.receipt_printer_name.trim()));
    }
    lines.push(String::new());
    lines.push("You can change any of these settings at any time from the Manager dashboard.".into());
    lines.join("\n")
}

/// Shows the first-run wizard and returns true once setup has been saved.
/// Nothing is written until the last page, so closing the window early leaves a clean slate
/// and the wizard simply shows again next time.
fn run_wizard(users: Arc<UsersDb>, config: Arc<ConfigDb>) -> Result<bool, slint::PlatformError> {
    let win = SetupWindow::new()?;
    win.set_version_text(VERSION.into());
    let finished = Arc::new(AtomicBool::new(false));

    let weak = win.as_weak();
    win.on_back(move || {
        let Some(win) = weak.upgrade() else { return };
        win.set_error_text("".into());
        win.set_page((win.get_page() - 1).max(0));
    });

    let (weak, finished_flag) = (win.as_weak(), finished.clone());
    win.on_next(move || {
        let Some(win) = weak.upgrade() else { return };
        if win.get_busy() {
            return;
        }
        let page = win.get_page();

        if page < LAST_PAGE {
            if let Err(e) = validate_setup_page(&win, page) {
                win.set_error_text(e.to_string().into());
                return;
            }
            win.set_error_text("".into());
            if page + 1 == LAST_PAGE {
                win.set_summary_text(setup_summary(&win).into());
            }
            win.set_page(page + 1);
            return;
        }

        // Last page: save everything. Argon2 is slow on purpose, so use a background thread.
        win.set_busy(true);
        win.set_error_text("".into());
        let data = collect_setup(&win);
        let (users, config, weak, finished_flag) =
            (users.clone(), config.clone(), win.as_weak(), finished_flag.clone());
        std::thread::spawn(move || {
            let result = run_setup(&*config, &*users, &data);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(win) = weak.upgrade() else { return };
                win.set_busy(false);
                match result {
                    Ok(()) => {
                        finished_flag.store(true, Ordering::SeqCst);
                        let _ = win.hide();
                    }
                    Err(e) => win.set_error_text(e.to_string().into()),
                }
            });
        });
    });

    win.run()?;
    Ok(finished.load(Ordering::SeqCst))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let users = Arc::new(UsersDb::open(dir.join("users.db"))?);
    let config = Arc::new(ConfigDb::open(dir.join("config.db"))?);
    let products = Arc::new(ProductsDb::open(dir.join("products.db"))?);
    let ctx = Arc::new(supervisor::Ctx { config: config.clone(), products });

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("add-user") {
        return add_user(&users, &args[1..]).map_err(Into::into);
    }

    // First run: no manager account yet, so walk through the setup wizard first.
    if needs_setup(&*users)? && !run_wizard(users.clone(), config.clone())? {
        return Ok(()); // closed without finishing
    }

    let auth = Arc::new(AuthService::new(users.clone())?);
    let win = LoginWindow::new()?;

    // Business name / subtitle / zoom come from config.db, like the Python login card.
    let biz = config.business()?;
    let name = biz
        .name
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let name = if name.is_empty() { "Merchant POS Systems".to_string() } else { name };
    win.set_title_size(fit_title_px(&name, 296.0));
    win.set_business_name(name.into());
    win.set_business_subtitle(biz.subtitle.split_whitespace().collect::<Vec<_>>().join(" ").into());
    win.set_version_text(VERSION.into());
    win.set_zoom_pct(config.get_int("ui_scale_pct", 100)?.clamp(70, 150) as f32);

    // The Python fields force upper case; the check is case-insensitive anyway.
    let weak = win.as_weak();
    win.on_username_edited(move |t| {
        let up = t.to_uppercase();
        if let (Some(w), true) = (weak.upgrade(), up != t.as_str()) {
            w.set_username(up.into());
        }
    });
    let weak = win.as_weak();
    win.on_password_edited(move |t| {
        let up = t.to_uppercase();
        if let (Some(w), true) = (weak.upgrade(), up != t.as_str()) {
            w.set_password(up.into());
        }
    });

    let (weak, config_z) = (win.as_weak(), config.clone());
    win.on_zoom_changed(move |pct| {
        let pct = pct.round().clamp(70.0, 150.0);
        if let Some(w) = weak.upgrade() {
            w.set_zoom_pct(pct);
        }
        let _ = config_z.set("ui_scale_pct", &(pct as i64).to_string());
    });

    let weak = win.as_weak();
    win.on_submit(move || {
        let Some(win) = weak.upgrade() else { return };
        let (username, password) = (win.get_username().to_string(), win.get_password().to_string());

        if username.trim().is_empty() {
            win.set_error_text("Please enter your username.".into());
            return;
        }
        if password.trim().is_empty() {
            win.set_error_text("Please enter your password.".into());
            return;
        }
        win.set_busy(true);
        win.set_error_text("".into());

        // Argon2 is deliberately slow: keep it off the UI thread.
        let (auth, users, config, weak, ctx) = (auth.clone(), users.clone(), config.clone(), win.as_weak(), ctx.clone());
        std::thread::spawn(move || {
            let result: Result<User, AuthError> = auth
                .login(&username, &password)
                .and_then(|user| check_session_gate(&*config, &*users, &user).map(|_| user));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(win) = weak.upgrade() else { return };
                win.set_busy(false);
                match result {
                    Ok(user) => on_signed_in(&win, user, ctx),
                    Err(e) => {
                        win.set_error_text(e.to_string().into());
                        if matches!(e, AuthError::InvalidCredentials) {
                            win.set_password("".into());
                        }
                    }
                }
            });
        });
    });

    win.run()?;
    Ok(())
}

thread_local! {
    // The signed-in window must stay alive while it is shown. Replaced on the next sign-in.
    static OPEN_WINDOW: std::cell::RefCell<Option<SupervisorWindow>> = const { std::cell::RefCell::new(None) };
}

/// Opens the window for `user.role` and hides the login window; Logout brings the login back.
/// Cashiers don't have a window yet (the cashier screen is the next big piece).
fn on_signed_in(win: &LoginWindow, user: User, ctx: Arc<supervisor::Ctx>) {
    println!("Signed in: {} ({})", user.full_name, user.role);
    win.set_username("".into());
    win.set_password("".into());

    if user.role == Role::Cashier {
        win.set_info_text(format!("Signed in as {} (cashier). The cashier window comes next.", user.full_name).into());
        return;
    }
    let login = win.as_weak();
    let present_ctx = ctx.clone();
    match supervisor::open(&user, ctx, move || {
        // Show the login window first so the event loop never sees zero open windows.
        if let Some(l) = login.upgrade() {
            let _ = l.show();
            l.invoke_focus_username();
        }
        OPEN_WINDOW.with(|w| {
            if let Some(w) = w.borrow().as_ref() {
                let _ = w.hide();
            }
        });
    }) {
        Ok(sup) => {
            if let Err(e) = supervisor::present(&sup, &present_ctx) {
                win.set_error_text(format!("Could not show the window: {e}").into());
                return;
            }
            let _ = win.hide();
            OPEN_WINDOW.with(|w| *w.borrow_mut() = Some(sup));
        }
        Err(e) => win.set_error_text(format!("Could not open the window: {e}").into()),
    }
}
