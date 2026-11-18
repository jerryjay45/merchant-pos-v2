# Merchant POS (Rust)

Rewrite of POS-Second-time. Workspace layout:

- `crates/pos-core` - business logic, no UI or DB driver (auth, session gate, role navigation, money, product paging, storage traits)
- `crates/pos-db`   - SQLite repositories: users, config, products (a PostgreSQL implementation will sit beside it)
- `crates/pos-app`  - Slint UI (first-run setup wizard, login window, supervisor/manager window)

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

Supervisor / manager window: after login, supervisors and managers get the areas their role allows
(`pos-core/src/nav.rs`, the same list as the Python tab registry). Inventory > Products is built: search by
name, barcode, alias group or variant group, 50 per page, prices resolved through the group rules. The other
pages show a "still in the Python app" note until they are ported. Products are read-only for now.

`products_schema.sql` and `products_fts.sql` in `pos-db` are copies of the Python schema; keep them identical,
since both apps read the same `products.db`. To cross-check the Rust list against Python on a real file:

    PRODUCTS_DB_FIXTURE=<products.db> PRODUCTS_QUERIES=<one search per line> PRODUCTS_OUT=<out.tsv> \
        cargo test -p pos-db python_fixture -- --ignored

