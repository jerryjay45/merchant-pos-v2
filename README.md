# Merchant POS (Rust)

Rewrite of POS-Second-time. Workspace layout:

- `crates/pos-core` - business logic, no UI or DB driver (auth, session gate, storage traits)
- `crates/pos-db`   - SQLite repositories (a PostgreSQL implementation will sit beside it)
- `crates/pos-app`  - Slint UI (first-run setup wizard, login window)

Data folder matches the Python app: `<home>/data/{users,config,products,checkout}.db`.
`<home>` is `MERCHANTPOS_HOME`, else `C:\MerchantPOS` on Windows, else `$XDG_DATA_HOME/merchantpos`.
Point `MERCHANTPOS_HOME` at an existing Python data folder to log in with real users.

    cargo run -p pos-app                                 # setup wizard on first run, then login
    cargo run -p pos-app -- add-user sam cashier "Sam"   # dev helper for extra users
    cargo test -p pos-core -p pos-db

Passwords: the Python app's salted-SHA-256 hashes still verify, new/changed passwords use Argon2
(which the Python app cannot read). Usernames and passwords are upper-cased before use, as in Python.

First run: while no manager account exists the setup wizard shows (business info, terminal ID,
tax & currency, manager account, printer). Nothing is written until the last page, and then the
business row and settings are saved in one transaction before the manager account is created.
Closing the wizard early leaves the data folder untouched and the wizard shows again next time.
