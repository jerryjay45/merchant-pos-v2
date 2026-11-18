//! First-run setup (port of ui/setup_wizard.py). The UI collects a `SetupData`, calls the
//! `validate_*` functions page by page, and finally `run_setup`, which stores everything.
//! No UI or database driver in here.

use thiserror::Error;

use crate::{
    auth::{new_password_hash, normalize_username, AuthError},
    repo::{BusinessInfo, ConfigRepository, StorageError, UserRepository},
    user::Role,
};

pub const MIN_PASSWORD_LEN: usize = 4;
pub const MAX_TERMINAL_ID_LEN: usize = 10;
pub const MAX_GCT_RATE_PCT: f64 = 50.0;
pub const MAX_RECEIPT_COPIES: u8 = 5;

#[derive(Debug, Error)]
pub enum SetupError {
    /// A message meant to be shown as-is next to the wizard page.
    #[error("{0}")]
    Invalid(String),
    #[error("A manager account already exists, so setup can't run again.")]
    AlreadySetUp,
    #[error("Could not save: {0}")]
    Storage(#[from] StorageError),
    #[error("Could not save: {0}")]
    Hash(#[from] AuthError),
}

fn invalid<T>(msg: &str) -> Result<T, SetupError> {
    Err(SetupError::Invalid(msg.to_string()))
}

/// Everything the wizard collects. Text is stored as typed; `run_setup` applies the same
/// upper-casing rules as the Python app.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupData {
    pub business: BusinessInfo,
    pub terminal_id: String,
    pub currency_symbol: String,
    pub gct_enabled: bool,
    /// Percent as typed, e.g. 16.5 (stored as the fraction 0.165, like the Python app).
    pub gct_rate_pct: f64,
    pub manager_full_name: String,
    pub manager_username: String,
    pub manager_password: String,
    /// Blank = the OS default printer.
    pub receipt_printer_name: String,
    pub receipt_copies: u8,
}

impl Default for SetupData {
    fn default() -> Self {
        Self {
            business: BusinessInfo {
                receipt_footer: "THANK YOU FOR YOUR BUSINESS!".into(),
                ..BusinessInfo::default()
            },
            terminal_id: String::new(),
            currency_symbol: "$".into(),
            gct_enabled: true,
            gct_rate_pct: 16.5,
            manager_full_name: String::new(),
            manager_username: String::new(),
            manager_password: String::new(),
            receipt_printer_name: String::new(),
            receipt_copies: 1,
        }
    }
}

/// True on a fresh install: the wizard runs while no manager account exists.
pub fn needs_setup<U: UserRepository + ?Sized>(users: &U) -> Result<bool, StorageError> {
    Ok(users.manager_count()? == 0)
}

// ───────────────────────────── page validation ─────────────────────────────

pub fn validate_business(name: &str) -> Result<(), SetupError> {
    if name.trim().is_empty() {
        return invalid("Business Name is required.");
    }
    Ok(())
}

pub fn validate_terminal_id(id: &str) -> Result<(), SetupError> {
    let id = id.trim();
    if id.is_empty() {
        return invalid("Terminal ID is required.");
    }
    let letters_and_digits = id.chars().filter(|c| *c != '-').count() > 0
        && id.chars().all(|c| c == '-' || c.is_alphanumeric());
    if !letters_and_digits {
        return invalid("Terminal ID must contain only letters and numbers.");
    }
    if id.chars().count() > MAX_TERMINAL_ID_LEN {
        return invalid("Terminal ID can be at most 10 characters.");
    }
    Ok(())
}

/// `rate_text` is what the user typed in the GCT rate box.
pub fn parse_gct_rate(rate_text: &str) -> Result<f64, SetupError> {
    let t = rate_text.trim().trim_end_matches('%').trim();
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() && (0.0..=MAX_GCT_RATE_PCT).contains(&v) => Ok(v),
        _ => invalid("GCT rate must be a number between 0 and 50."),
    }
}

pub fn validate_manager(
    full_name: &str,
    username: &str,
    password: &str,
    confirm: &str,
) -> Result<(), SetupError> {
    if full_name.trim().is_empty() {
        return invalid("Full Name is required.");
    }
    if username.trim().is_empty() {
        return invalid("Username is required.");
    }
    if password.trim().is_empty() {
        return invalid("Password is required.");
    }
    if password.trim().chars().count() < MIN_PASSWORD_LEN {
        return invalid("Password must be at least 4 characters.");
    }
    // Passwords are case-insensitive (upper-cased before use), so compare them that way.
    if password.trim().to_uppercase() != confirm.trim().to_uppercase() {
        return invalid("Passwords do not match.");
    }
    Ok(())
}

// ───────────────────────────── normalisation ─────────────────────────────

/// Trim each line and drop empty ones (one typed line = one printed line).
pub fn clean_lines(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Same rules as `update_business` in core/db_config.py: everything upper-case except the
/// email; name and address keep their lines.
pub fn normalize_business(b: &BusinessInfo) -> BusinessInfo {
    BusinessInfo {
        name: clean_lines(&b.name).to_uppercase(),
        address: clean_lines(&b.address).to_uppercase(),
        phone: b.phone.trim().to_uppercase(),
        email: b.email.trim().to_string(),
        tax_id: b.tax_id.trim().to_uppercase(),
        receipt_footer: b.receipt_footer.trim().to_uppercase(),
    }
}

fn rate_to_setting(rate_pct: f64) -> String {
    // 16.5 -> "0.165", 7 -> "0.07", like str(value / 100) in Python.
    format!("{}", (rate_pct * 100.0).round() / 10_000.0)
}

// ───────────────────────────── apply ─────────────────────────────

/// Validates everything again, then stores it. Settings and business details go in one
/// transaction, and the manager account is created last: if anything fails before that the
/// wizard simply shows again, and no half-finished install is left without a way in.
pub fn run_setup<C, U>(config: &C, users: &U, data: &SetupData) -> Result<(), SetupError>
where
    C: ConfigRepository + ?Sized,
    U: UserRepository + ?Sized,
{
    validate_business(&data.business.name)?;
    validate_terminal_id(&data.terminal_id)?;
    if !(0.0..=MAX_GCT_RATE_PCT).contains(&data.gct_rate_pct) {
        return invalid("GCT rate must be a number between 0 and 50.");
    }
    validate_manager(
        &data.manager_full_name,
        &data.manager_username,
        &data.manager_password,
        &data.manager_password,
    )?;
    if users.manager_count()? > 0 {
        return Err(SetupError::AlreadySetUp);
    }

    let currency = data.currency_symbol.trim();
    let copies = data.receipt_copies.clamp(1, MAX_RECEIPT_COPIES);
    let mut settings = vec![
        ("terminal_id".to_string(), data.terminal_id.trim().to_uppercase()),
        ("currency_symbol".to_string(), if currency.is_empty() { "$" } else { currency }.to_string()),
        ("gct_enabled".to_string(), if data.gct_enabled { "1" } else { "0" }.to_string()),
        ("gct_rate".to_string(), rate_to_setting(data.gct_rate_pct)),
        ("receipt_copies".to_string(), copies.to_string()),
    ];
    let printer = data.receipt_printer_name.trim();
    if !printer.is_empty() {
        settings.push(("receipt_printer_name".to_string(), printer.to_string()));
    }
    config.apply_setup(&normalize_business(&data.business), &settings)?;

    // Argon2 is slow on purpose; callers should run this on a background thread.
    let hash = new_password_hash(&data.manager_password)?;
    users.insert_user(
        &data.manager_full_name.trim().to_uppercase(),
        &normalize_username(&data.manager_username),
        &hash,
        Role::Manager,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(r: Result<(), SetupError>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn business_name_is_required() {
        assert!(validate_business(" JOHN'S ").is_ok());
        assert_eq!(msg(validate_business("  ")), "Business Name is required.");
    }

    #[test]
    fn terminal_id_rules_match_python() {
        for ok in ["T01", "main", "COUNTER2", "T-01", "ABCDEFGHIJ"] {
            assert!(validate_terminal_id(ok).is_ok(), "{ok}");
        }
        assert_eq!(msg(validate_terminal_id("")), "Terminal ID is required.");
        assert!(validate_terminal_id("-").is_err());
        assert!(validate_terminal_id("T 01").is_err());
        assert!(validate_terminal_id("T01!").is_err());
        assert!(validate_terminal_id("ABCDEFGHIJK").is_err());
    }

    #[test]
    fn gct_rate_parsing() {
        assert_eq!(parse_gct_rate("16.5").unwrap(), 16.5);
        assert_eq!(parse_gct_rate(" 7 % ").unwrap(), 7.0);
        assert_eq!(parse_gct_rate("0").unwrap(), 0.0);
        assert!(parse_gct_rate("51").is_err());
        assert!(parse_gct_rate("-1").is_err());
        assert!(parse_gct_rate("abc").is_err());
        assert!(parse_gct_rate("NaN").is_err());
        assert_eq!(rate_to_setting(16.5), "0.165");
        assert_eq!(rate_to_setting(7.0), "0.07");
        assert_eq!(rate_to_setting(0.0), "0");
    }

    #[test]
    fn manager_page_checks_in_python_order() {
        assert_eq!(msg(validate_manager("", "u", "pass", "pass")), "Full Name is required.");
        assert_eq!(msg(validate_manager("J", "", "pass", "pass")), "Username is required.");
        assert_eq!(msg(validate_manager("J", "u", "", "")), "Password is required.");
        assert_eq!(msg(validate_manager("J", "u", "abc", "abc")), "Password must be at least 4 characters.");
        assert_eq!(msg(validate_manager("J", "u", "abcd", "abce")), "Passwords do not match.");
        assert!(validate_manager("J", "u", "abcd", "ABCD").is_ok(), "case-insensitive, like login");
    }

    #[test]
    fn business_is_normalised_like_python() {
        let b = normalize_business(&BusinessInfo {
            name: " john's \n super market ".into(),
            address: "12 main street\n\n  kingston ".into(),
            phone: "876-555-0100".into(),
            email: " Info@Shop.com ".into(),
            tax_id: "abc-123".into(),
            receipt_footer: "thanks!".into(),
        });
        assert_eq!(b.name, "JOHN'S\nSUPER MARKET");
        assert_eq!(b.address, "12 MAIN STREET\nKINGSTON");
        assert_eq!(b.email, "Info@Shop.com");
        assert_eq!(b.tax_id, "ABC-123");
        assert_eq!(b.receipt_footer, "THANKS!");
    }
}
