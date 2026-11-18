use std::{fmt, str::FromStr};

/// Same three roles (and spellings) as the `users.role` CHECK constraint in the Python app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Cashier,
    Supervisor,
    Manager,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Cashier => "cashier",
            Role::Supervisor => "supervisor",
            Role::Manager => "manager",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cashier" => Ok(Role::Cashier),
            "supervisor" => Ok(Role::Supervisor),
            "manager" => Ok(Role::Manager),
            other => Err(format!("unknown role: {other:?}")),
        }
    }
}

/// A signed-in user. Deliberately has no password hash, so it is safe to hand to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub full_name: String,
    pub username: String,
    pub role: Role,
    pub is_active: bool,
}

/// What the repository returns: a user plus the stored hash (never leaves `pos-core`/`pos-db`).
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub user: User,
    pub password_hash: String,
}
