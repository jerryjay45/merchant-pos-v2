use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    repo::{ConfigRepository, SessionRepository, StorageError, UserRepository},
    user::{Role, User},
};

#[derive(Debug, Error)]
pub enum AuthError {
    /// Wrong username or password. Deliberately the same message for both.
    #[error("Invalid username or password.")]
    InvalidCredentials,
    #[error("This account is disabled. Ask your manager.")]
    Inactive,
    #[error("Please enter your username and password.")]
    EmptyInput,
    #[error("No active session. Ask your supervisor to open one before you log in.")]
    NoActiveSession,
    #[error("Could not read the database: {0}")]
    Storage(#[from] StorageError),
    #[error("Password hashing failed: {0}")]
    Hash(String),
}

// The Python app upper-cases usernames and passwords (both when saving and when checking),
// so logins are case-insensitive. Keep that, or existing hashes would stop matching.
pub fn normalize_username(s: &str) -> String {
    s.trim().to_uppercase()
}
fn normalize_password(s: &str) -> String {
    s.trim().to_uppercase()
}

/// Hash for a NEW or changed password (Argon2id). The Python app cannot read these.
pub fn new_password_hash(raw_password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(normalize_password(raw_password).as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// The Python app's format: `<salt_hex>:<sha256_hex(salt + PASSWORD)>`.
pub fn legacy_password_hash(raw_password: &str, salt_hex: &str) -> String {
    format!("{salt_hex}:{}", legacy_digest(salt_hex, &normalize_password(raw_password)))
}

fn legacy_digest(salt: &str, normalized: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(normalized.as_bytes());
    format!("{:x}", h.finalize())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Accepts both Argon2 hashes and the legacy salted-SHA-256 format.
pub fn verify_password(raw_password: &str, stored: &str) -> bool {
    let pw = normalize_password(raw_password);
    if stored.starts_with("$argon2") {
        return PasswordHash::new(stored)
            .map(|p| Argon2::default().verify_password(pw.as_bytes(), &p).is_ok())
            .unwrap_or(false);
    }
    match stored.split_once(':') {
        Some((salt, digest)) => constant_time_eq(&legacy_digest(salt, &pw), digest),
        None => false,
    }
}

pub struct AuthService<R: UserRepository> {
    repo: R,
    /// Verified against when the username doesn't exist, so response time doesn't reveal it.
    dummy_hash: String,
}

impl<R: UserRepository> AuthService<R> {
    pub fn new(repo: R) -> Result<Self, AuthError> {
        Ok(Self { repo, dummy_hash: new_password_hash("not-a-real-password")? })
    }

    pub fn repo(&self) -> &R {
        &self.repo
    }

    pub fn login(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let username = normalize_username(username);
        if username.is_empty() || password.trim().is_empty() {
            return Err(AuthError::EmptyInput);
        }

        match self.repo.find_by_username(&username)? {
            None => {
                let _ = verify_password(password, &self.dummy_hash);
                Err(AuthError::InvalidCredentials)
            }
            Some(rec) => {
                if !verify_password(password, &rec.password_hash) {
                    return Err(AuthError::InvalidCredentials);
                }
                if !rec.user.is_active {
                    return Err(AuthError::Inactive);
                }
                Ok(rec.user)
            }
        }
    }
}

/// Cashiers may be blocked from signing in until a supervisor has opened a session for them
/// (the `session_gate` setting).
pub fn check_session_gate<C, S>(config: &C, sessions: &S, user: &User) -> Result<(), AuthError>
where
    C: ConfigRepository + ?Sized,
    S: SessionRepository + ?Sized,
{
    if user.role == Role::Cashier
        && config.get_bool("session_gate", false)?
        && !sessions.has_open_session(user.id)?
    {
        return Err(AuthError::NoActiveSession);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::UserRecord;

    struct Fake(Vec<UserRecord>);
    impl UserRepository for Fake {
        fn find_by_username(&self, u: &str) -> Result<Option<UserRecord>, StorageError> {
            Ok(self.0.iter().find(|r| r.user.username.eq_ignore_ascii_case(u)).cloned())
        }
        fn user_count(&self) -> Result<u64, StorageError> {
            Ok(self.0.len() as u64)
        }
        fn manager_count(&self) -> Result<u64, StorageError> {
            Ok(self.0.iter().filter(|r| r.user.role == Role::Manager).count() as u64)
        }
        fn insert_user(&self, _: &str, _: &str, _: &str, _: Role) -> Result<i64, StorageError> {
            unimplemented!("not needed by these tests")
        }
    }

    fn rec(name: &str, hash: String, active: bool) -> UserRecord {
        UserRecord {
            user: User {
                id: 1,
                full_name: name.into(),
                username: name.into(),
                role: Role::Cashier,
                is_active: active,
            },
            password_hash: hash,
        }
    }

    #[test]
    fn verifies_a_hash_made_by_the_python_app() {
        // Generated with the Python code: sha256(salt + "S3CRET")
        let stored = "00112233445566778899aabbccddeeff:386292eb97717b4f1789319f85a170896a86b109b034af7ca44c1002fd22c2a3";
        assert!(verify_password("S3CRET", stored));
        assert!(verify_password("  s3cret ", stored), "case and spaces are ignored, as in Python");
        assert!(!verify_password("S3CRET1", stored));
        assert_eq!(legacy_password_hash("s3cret", "00112233445566778899aabbccddeeff"), stored);
    }

    #[test]
    fn argon2_round_trip() {
        let h = new_password_hash("Pw123").unwrap();
        assert!(h.starts_with("$argon2"));
        assert!(verify_password("PW123", &h));
        assert!(!verify_password("PW124", &h));
    }

    #[test]
    fn login_is_case_insensitive_and_hides_which_part_was_wrong() {
        let svc = AuthService::new(Fake(vec![rec("JAY", new_password_hash("pw123").unwrap(), true)])).unwrap();
        assert_eq!(svc.login(" jay ", "PW123").unwrap().username, "JAY");
        assert!(matches!(svc.login("jay", "nope"), Err(AuthError::InvalidCredentials)));
        assert!(matches!(svc.login("ghost", "pw123"), Err(AuthError::InvalidCredentials)));
    }

    #[test]
    fn inactive_and_empty() {
        let svc = AuthService::new(Fake(vec![rec("OLD", new_password_hash("pw").unwrap(), false)])).unwrap();
        assert!(matches!(svc.login("old", "pw"), Err(AuthError::Inactive)));
        assert!(matches!(svc.login("", "pw"), Err(AuthError::EmptyInput)));
        assert!(matches!(svc.login("old", "  "), Err(AuthError::EmptyInput)));
    }
}
