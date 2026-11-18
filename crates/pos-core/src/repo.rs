//! Storage traits. `pos-db` implements them for SQLite; PostgreSQL can follow
//! without any change to the UI or to `pos-core`.

use std::sync::Arc;

use thiserror::Error;

use crate::user::{Role, UserRecord};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct StorageError(pub String);

pub trait UserRepository: Send + Sync {
    /// Case-insensitive lookup. Returns inactive users too; the caller decides.
    fn find_by_username(&self, username: &str) -> Result<Option<UserRecord>, StorageError>;
    fn user_count(&self) -> Result<u64, StorageError>;
    /// Active or not; the setup wizard only runs while this is zero.
    fn manager_count(&self) -> Result<u64, StorageError>;
    /// Stores the user exactly as given (callers normalise names and hash the password).
    fn insert_user(
        &self,
        full_name: &str,
        username: &str,
        password_hash: &str,
        role: Role,
    ) -> Result<i64, StorageError>;
}

pub trait SessionRepository: Send + Sync {
    fn has_open_session(&self, user_id: i64) -> Result<bool, StorageError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Business {
    /// May contain several lines (one typed line = one printed line).
    pub name: String,
    pub subtitle: String,
}

/// The full `business` table row (what the setup wizard and receipts use).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BusinessInfo {
    pub name: String,
    pub address: String,
    pub phone: String,
    pub email: String,
    pub tax_id: String,
    pub receipt_footer: String,
}

pub trait ConfigRepository: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError>;
    fn set(&self, key: &str, value: &str) -> Result<(), StorageError>;
    fn business(&self) -> Result<Business, StorageError>;

    /// Writes the business row and the settings in ONE transaction: either everything is
    /// applied or, on any error, nothing is. Empty business fields are left unchanged,
    /// like `update_business` in the Python app.
    fn apply_setup(
        &self,
        business: &BusinessInfo,
        settings: &[(String, String)],
    ) -> Result<(), StorageError>;

    fn get_bool(&self, key: &str, default: bool) -> Result<bool, StorageError> {
        Ok(match self.get(key)? {
            Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => default,
        })
    }

    fn get_int(&self, key: &str, default: i64) -> Result<i64, StorageError> {
        Ok(self.get(key)?.and_then(|v| v.trim().parse().ok()).unwrap_or(default))
    }
}

// Lets the app share one connection between the auth service and the session check.
impl<T: UserRepository + ?Sized> UserRepository for Arc<T> {
    fn find_by_username(&self, u: &str) -> Result<Option<UserRecord>, StorageError> {
        (**self).find_by_username(u)
    }
    fn user_count(&self) -> Result<u64, StorageError> {
        (**self).user_count()
    }
    fn manager_count(&self) -> Result<u64, StorageError> {
        (**self).manager_count()
    }
    fn insert_user(&self, n: &str, u: &str, h: &str, r: Role) -> Result<i64, StorageError> {
        (**self).insert_user(n, u, h, r)
    }
}
impl<T: SessionRepository + ?Sized> SessionRepository for Arc<T> {
    fn has_open_session(&self, id: i64) -> Result<bool, StorageError> {
        (**self).has_open_session(id)
    }
}
impl<T: ConfigRepository + ?Sized> ConfigRepository for Arc<T> {
    fn get(&self, k: &str) -> Result<Option<String>, StorageError> {
        (**self).get(k)
    }
    fn set(&self, k: &str, v: &str) -> Result<(), StorageError> {
        (**self).set(k, v)
    }
    fn business(&self) -> Result<Business, StorageError> {
        (**self).business()
    }
    fn apply_setup(&self, b: &BusinessInfo, s: &[(String, String)]) -> Result<(), StorageError> {
        (**self).apply_setup(b, s)
    }
}
