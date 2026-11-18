//! Storage traits. `pos-db` implements them for SQLite; PostgreSQL can follow
//! without any change to the UI or to `pos-core`.

use std::sync::Arc;

use thiserror::Error;

use crate::user::UserRecord;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct StorageError(pub String);

pub trait UserRepository: Send + Sync {
    /// Case-insensitive lookup. Returns inactive users too; the caller decides.
    fn find_by_username(&self, username: &str) -> Result<Option<UserRecord>, StorageError>;
    fn user_count(&self) -> Result<u64, StorageError>;
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

pub trait ConfigRepository: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError>;
    fn set(&self, key: &str, value: &str) -> Result<(), StorageError>;
    fn business(&self) -> Result<Business, StorageError>;

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
}
