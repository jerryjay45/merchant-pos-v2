# Merchant POS (Rust)

Rewrite of POS-Second-time. Workspace layout:

- `crates/pos-core` - business logic, no UI or DB driver (auth, session gate, storage traits)
- `crates/pos-db`   - SQLite repositories (a PostgreSQL implementation will sit beside it)
- `crates/pos-app`  - Slint UI (login window so far)

Data folder matches the Python app: `<home>/data/{users,config,products,checkout}.db`.
`<home>` is `MERCHANTPOS_HOME`, else `C:\MerchantPOS` on Windows, else `$XDG_DATA_HOME/merchantpos`.
Point `MERCHANTPOS_HOME` at an existing Python data folder to log in with real users.

    cargo run -p pos-app -- add-user jay manager "Jay"   # once, on an empty data folder
    cargo run -p pos-app                                 # login window
    cargo test -p pos-core -p pos-db

Passwords: the Python app's salted-SHA-256 hashes still verify, new/changed passwords use Argon2
(which the Python app cannot read). Usernames and passwords are upper-cased before use, as in Python.
