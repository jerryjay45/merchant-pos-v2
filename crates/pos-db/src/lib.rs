//! SQLite implementation of the repository traits in `pos-core`.
//! File layout matches the Python app: `<data dir>/users.db` and `<data dir>/config.db`,
//! so the Rust app can be pointed at an existing data folder.

use std::{path::Path, sync::Mutex};

use pos_core::{
    repo::{Business, BusinessInfo, ConfigRepository, SessionRepository, StorageError, UserRepository},
    user::{Role, User, UserRecord},
};
use rusqlite::{params, Connection, OptionalExtension};

fn storage<E: std::fmt::Display>(e: E) -> StorageError {
    StorageError(e.to_string())
}

fn configure(conn: &Connection) -> Result<(), StorageError> {
    // journal_mode returns a row, so it has to go through query_row.
    let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
    conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")
        .map_err(storage)
}

// ───────────────────────────── users.db ─────────────────────────────

// Copied from core/db_users.py. The cash-log, paid-out and drawer tables come with the
// cashier work; the Python app's migrations add any later columns to existing files.
const USERS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    full_name   TEXT    NOT NULL,
    username    TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT  NOT NULL,
    role        TEXT    NOT NULL CHECK(role IN ('cashier','supervisor','manager')),
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),
    updated_at  TEXT    NOT NULL DEFAULT (datetime('now', 'localtime'))
);
CREATE TABLE IF NOT EXISTS sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    opened_by   INTEGER DEFAULT NULL REFERENCES users(id) ON DELETE SET NULL,
    closed_by   INTEGER DEFAULT NULL REFERENCES users(id) ON DELETE SET NULL,
    opened_at   TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),
    closed_at   TEXT    DEFAULT NULL,
    total_sales REAL    NOT NULL DEFAULT 0.0,
    status      TEXT    NOT NULL DEFAULT 'open' CHECK(status IN ('open','closed'))
);
CREATE INDEX IF NOT EXISTS idx_users_username ON users(username COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);
";

pub struct UsersDb {
    conn: Mutex<Connection>,
}

impl UsersDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::init(Connection::open(path).map_err(storage)?)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory().map_err(storage)?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        configure(&conn)?;
        conn.execute_batch(USERS_SCHEMA).map_err(storage)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Test/setup helper: open a session for a cashier.
    pub fn open_session(&self, user_id: i64) -> Result<(), StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute("INSERT INTO sessions (user_id) VALUES (?1)", params![user_id])
            .map_err(storage)?;
        Ok(())
    }
}

impl UserRepository for UsersDb {
    fn find_by_username(&self, username: &str) -> Result<Option<UserRecord>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        let row = conn
            .query_row(
                "SELECT id, full_name, username, password_hash, role, is_active
                 FROM users WHERE username = ?1 COLLATE NOCASE",
                params![username],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(storage)?;

        row.map(|(id, full_name, username, password_hash, role, active)| {
            let role = role.parse::<Role>().map_err(StorageError)?;
            Ok(UserRecord {
                user: User { id, full_name, username, role, is_active: active != 0 },
                password_hash,
            })
        })
        .transpose()
    }

    fn user_count(&self) -> Result<u64, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(storage)
    }

    fn manager_count(&self) -> Result<u64, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row("SELECT COUNT(*) FROM users WHERE role = 'manager'", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(storage)
    }

    /// `username` is stored upper-case, like the Python app does.
    fn insert_user(
        &self,
        full_name: &str,
        username: &str,
        password_hash: &str,
        role: Role,
    ) -> Result<i64, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute(
            "INSERT INTO users (full_name, username, password_hash, role, is_active)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![full_name.trim(), username.trim().to_uppercase(), password_hash, role.as_str()],
        )
        .map_err(storage)?;
        Ok(conn.last_insert_rowid())
    }
}

impl SessionRepository for UsersDb {
    fn has_open_session(&self, user_id: i64) -> Result<bool, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row(
            "SELECT 1 FROM sessions WHERE user_id = ?1 AND status = 'open' LIMIT 1",
            params![user_id],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .map_err(storage)
    }
}

// ───────────────────────────── config.db ─────────────────────────────

const CONFIG_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS business (
    id              INTEGER PRIMARY KEY CHECK(id = 1),
    name            TEXT    NOT NULL DEFAULT 'My Business',
    address         TEXT    NOT NULL DEFAULT '',
    phone           TEXT    NOT NULL DEFAULT '',
    email           TEXT    NOT NULL DEFAULT '',
    tax_id          TEXT    NOT NULL DEFAULT '',
    receipt_footer  TEXT    NOT NULL DEFAULT 'Thank you for your business!',
    logo_path       TEXT    DEFAULT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
INSERT OR IGNORE INTO business (id) VALUES (1);
";

pub struct ConfigDb {
    conn: Mutex<Connection>,
}

impl ConfigDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::init(Connection::open(path).map_err(storage)?)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory().map_err(storage)?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        configure(&conn)?;
        conn.execute_batch(CONFIG_SCHEMA).map_err(storage)?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}

impl ConfigRepository for ConfigDb {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(storage)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(storage)?;
        Ok(())
    }

    fn business(&self) -> Result<Business, StorageError> {
        let name: Option<String> = {
            let conn = self.conn.lock().map_err(storage)?;
            conn.query_row("SELECT name FROM business WHERE id = 1", [], |r| r.get(0))
                .optional()
                .map_err(storage)?
        };
        let subtitle = self.get("business_subtitle")?.unwrap_or_default();
        Ok(Business { name: name.unwrap_or_default(), subtitle })
    }

    fn apply_setup(
        &self,
        business: &BusinessInfo,
        settings: &[(String, String)],
    ) -> Result<(), StorageError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        // Empty fields keep their current value (the Python wizard skips blank fields too).
        tx.execute(
            "UPDATE business SET
                name           = COALESCE(NULLIF(?1, ''), name),
                address        = COALESCE(NULLIF(?2, ''), address),
                phone          = COALESCE(NULLIF(?3, ''), phone),
                email          = COALESCE(NULLIF(?4, ''), email),
                tax_id         = COALESCE(NULLIF(?5, ''), tax_id),
                receipt_footer = COALESCE(NULLIF(?6, ''), receipt_footer)
             WHERE id = 1",
            params![
                business.name,
                business.address,
                business.phone,
                business.email,
                business.tax_id,
                business.receipt_footer
            ],
        )
        .map_err(storage)?;
        for (k, v) in settings {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pos_core::{auth::legacy_password_hash, check_session_gate, new_password_hash, AuthError, AuthService};
    use std::sync::Arc;

    #[test]
    fn logs_in_with_both_hash_formats() {
        let db = Arc::new(UsersDb::open_in_memory().unwrap());
        db.insert_user("Jay", "jay", &new_password_hash("secret").unwrap(), Role::Manager).unwrap();
        db.insert_user("Old", "old", &legacy_password_hash("pin1", "abcd"), Role::Cashier).unwrap();

        let svc = AuthService::new(db.clone()).unwrap();
        assert_eq!(svc.login("JAY", "Secret").unwrap().role, Role::Manager);
        assert_eq!(svc.login("old", "PIN1").unwrap().role, Role::Cashier);
        assert!(svc.login("jay", "wrong").is_err());
        assert_eq!(db.user_count().unwrap(), 2);
    }

    #[test]
    fn session_gate_only_blocks_cashiers_without_an_open_session() {
        let users = Arc::new(UsersDb::open_in_memory().unwrap());
        let config = ConfigDb::open_in_memory().unwrap();
        let cashier_id = users.insert_user("C", "c", "x:y", Role::Cashier).unwrap();
        let mgr_id = users.insert_user("M", "m", "x:y", Role::Manager).unwrap();
        let svc = AuthService::new(users.clone()).unwrap();
        let find = |name: &str| users.find_by_username(name).unwrap().unwrap().user;
        let (cashier, manager) = (find("C"), find("M"));
        assert_eq!((cashier.id, manager.id), (cashier_id, mgr_id));
        let _ = svc;

        // gate off (default): everyone gets in
        assert!(check_session_gate(&config, &*users, &cashier).is_ok());

        config.set("session_gate", "1").unwrap();
        assert!(matches!(check_session_gate(&config, &*users, &cashier), Err(AuthError::NoActiveSession)));
        assert!(check_session_gate(&config, &*users, &manager).is_ok());

        users.open_session(cashier_id).unwrap();
        assert!(check_session_gate(&config, &*users, &cashier).is_ok());
    }

    #[test]
    fn config_defaults_and_settings() {
        let config = ConfigDb::open_in_memory().unwrap();
        assert_eq!(config.business().unwrap().name, "My Business");
        assert_eq!(config.get_int("ui_scale_pct", 100).unwrap(), 100);
        config.set("ui_scale_pct", "120").unwrap();
        config.set("ui_scale_pct", "130").unwrap();
        assert_eq!(config.get_int("ui_scale_pct", 100).unwrap(), 130);
    }

    #[test]
    fn first_run_setup_creates_everything_and_only_runs_once() {
        use pos_core::{needs_setup, run_setup, SetupData, SetupError};
        let users = Arc::new(UsersDb::open_in_memory().unwrap());
        let config = ConfigDb::open_in_memory().unwrap();
        assert!(needs_setup(&*users).unwrap());

        let mut data = SetupData::default();
        data.business.name = "john's\nsupermarket".into();
        data.business.address = "12 main st\nkingston".into();
        data.business.email = "Info@Shop.com".into();
        data.terminal_id = "t01".into();
        data.currency_symbol = "J$".into();
        data.gct_rate_pct = 16.5;
        data.manager_full_name = "john smith".into();
        data.manager_username = "boss".into();
        data.manager_password = "pw1234".into();
        data.receipt_printer_name = "EPSON TM-U220".into();
        data.receipt_copies = 2;
        run_setup(&config, &*users, &data).unwrap();

        assert!(!needs_setup(&*users).unwrap());
        assert_eq!(config.business().unwrap().name, "JOHN'S\nSUPERMARKET");
        assert_eq!(config.get("terminal_id").unwrap().as_deref(), Some("T01"));
        assert_eq!(config.get("currency_symbol").unwrap().as_deref(), Some("J$"));
        assert_eq!(config.get("gct_rate").unwrap().as_deref(), Some("0.165"));
        assert_eq!(config.get("gct_enabled").unwrap().as_deref(), Some("1"));
        assert_eq!(config.get("receipt_copies").unwrap().as_deref(), Some("2"));
        assert_eq!(config.get("receipt_printer_name").unwrap().as_deref(), Some("EPSON TM-U220"));

        // the new manager can log in (case-insensitive) and has the manager role
        let svc = AuthService::new(users.clone()).unwrap();
        let u = svc.login("BOSS", "PW1234").unwrap();
        assert_eq!((u.role, u.full_name.as_str()), (Role::Manager, "JOHN SMITH"));

        // second run is refused and changes nothing
        data.business.name = "other".into();
        assert!(matches!(run_setup(&config, &*users, &data), Err(SetupError::AlreadySetUp)));
        assert_eq!(config.business().unwrap().name, "JOHN'S\nSUPERMARKET");
    }

    #[test]
    fn invalid_setup_writes_nothing() {
        use pos_core::{run_setup, SetupData};
        let users = UsersDb::open_in_memory().unwrap();
        let config = ConfigDb::open_in_memory().unwrap();
        let mut data = SetupData::default();
        data.terminal_id = "T01".into();
        data.manager_full_name = "J".into();
        data.manager_username = "j".into();
        data.manager_password = "pw1234".into();
        // business name missing
        assert!(run_setup(&config, &users, &data).is_err());
        assert_eq!(config.get("terminal_id").unwrap(), None);
        assert_eq!(users.user_count().unwrap(), 0);
    }
}
