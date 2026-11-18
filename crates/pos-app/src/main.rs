use std::{
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use pos_core::{
    check_session_gate, new_password_hash, AuthError, AuthService, ConfigRepository, Role, User,
    UserRepository,
};
use pos_db::{ConfigDb, UsersDb};
use slint::ComponentHandle;

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

/// Dev helper until the first-run setup wizard exists:
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let users = Arc::new(UsersDb::open(dir.join("users.db"))?);
    let config = Arc::new(ConfigDb::open(dir.join("config.db"))?);

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("add-user") {
        return add_user(&users, &args[1..]).map_err(Into::into);
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

    if users.user_count()? == 0 {
        win.set_info_text(
            "No users yet. Run `pos-app add-user <username> manager` once to create the first one."
                .into(),
        );
    }

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
        let (auth, users, config, weak) = (auth.clone(), users.clone(), config.clone(), win.as_weak());
        std::thread::spawn(move || {
            let result: Result<User, AuthError> = auth
                .login(&username, &password)
                .and_then(|user| check_session_gate(&*config, &*users, &user).map(|_| user));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(win) = weak.upgrade() else { return };
                win.set_busy(false);
                match result {
                    Ok(user) => on_signed_in(&win, user),
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

/// Next step: open the window for `user.role` (cashier / supervisor / manager),
/// hide this one, and show it again on logout.
fn on_signed_in(win: &LoginWindow, user: User) {
    println!("Signed in: {} ({})", user.full_name, user.role);
    win.set_username("".into());
    win.set_password("".into());
    win.set_info_text(
        format!("Signed in as {} ({}). The {} window comes next.", user.full_name, user.role, user.role)
            .into(),
    );
}
