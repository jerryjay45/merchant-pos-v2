//! Business logic for Merchant POS. No UI, no database driver in here:
//! everything the UI needs goes through the types and traits in this crate.

pub mod auth;
pub mod repo;
pub mod setup;
pub mod user;

pub use auth::{check_session_gate, new_password_hash, verify_password, AuthError, AuthService};
pub use repo::{Business, BusinessInfo, ConfigRepository, SessionRepository, StorageError, UserRepository};
pub use setup::{needs_setup, run_setup, SetupData, SetupError};
pub use user::{Role, User, UserRecord};
