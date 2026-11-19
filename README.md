# Merchant POS (Rust)

A native point-of-sale app for small shops, written in Rust with a [Slint](https://slint.dev) UI.
It is a rewrite of the Python/PyQt app (POS-Second-time), built to start fast and stay light on
older tills, and it reads and writes the **same data folder** as the Python app, so you can move
over one screen at a time.

## Status

| Area | State |
|---|---|
| First-run setup wizard | Done |
| Login (all roles) | Done |
| Supervisor / manager window: role-based areas and pages | Done |
| Inventory > Products: search, paging, add, edit, delete | Done, except case items (see below) |
| Product discounts: two tiers, optional dates, stacking | Done, in the product dialog |
| Inventory > Stock, Price Tags, Quick Keys, Groups & Discounts, Stock Settings | Not started |
| Applying discounts at checkout | Not started (needs the cashier window) |
| Case / pack pricing, group-level discounts, shared discount levels page | Not started |
| Sales, Printing, Data, Setup pages | Not started (the window shows a "still in the Python app" note) |
| Cashier window (checkout) | Not started: a cashier can sign in, then sees a note |
| PostgreSQL backend | Not started (SQLite only) |

## Build and run

You need Rust **1.95 or newer**: the Slint version pinned in `Cargo.lock` (1.19) requires it.
Commit `Cargo.lock`; this is an application.

    cargo run -p pos-app                                  # setup wizard on first run, then login
    cargo run -p pos-app -- add-user sam cashier "Sam"    # dev helper: add a user
    cargo test -p pos-core -p pos-db                      # the logic and database tests

To try the wizard again, use a throwaway data folder: `MERCHANTPOS_HOME=/tmp/mpos-test cargo run -p pos-app`.
The wizard only shows while the data folder has no manager account.

## Workspace

| Crate | What is in it |
|---|---|
| `crates/pos-core` | The rules, with **no UI and no database driver**: auth and hashing, the session gate, first-run setup validation, which pages each role sees (`nav.rs`), money (`money.rs`), product list paging (`product.rs`), product add/edit rules (`product_edit.rs`), and discount rules (`discount.rs`). Everything the UI needs goes through its types and traits, so it can be tested without a window. |
| `crates/pos-db` | SQLite repositories: users, config, products. A PostgreSQL implementation will sit beside them. |
| `crates/pos-app` | The Slint UI and the glue that moves data between the windows and the database: `login.slint`, `setup.slint`, `supervisor.slint`, `product_form.slint`, `group_chooser.slint`, and `main.rs`, `supervisor.rs`, `product_form.rs`. |

## Data folder

The same layout as the Python app: `<home>/data/{users,config,products,checkout}.db`.
`<home>` is `MERCHANTPOS_HOME` if set, otherwise `C:\MerchantPOS` on Windows, otherwise
`$XDG_DATA_HOME/merchantpos` (usually `~/.local/share/merchantpos`).

Point `MERCHANTPOS_HOME` at an existing Python data folder to use your real users and products.

- **Passwords.** The Python app's salted SHA-256 hashes still verify. New or changed passwords use
  Argon2, which the Python app cannot read, so avoid changing passwords until the Python app is retired.
  Usernames and passwords are upper-cased before use, as in Python.
- **Schema.** `crates/pos-db/src/products_schema.sql` and `products_fts.sql` are copies of the Python
  app's schema and search index. Keep them identical, because both apps read the same `products.db`.
- **Columns the Rust app adds.** On start, `pos-db` adds five optional columns to `products`
  (`discount1_starts`, `discount1_ends`, `discount2_starts`, `discount2_ends`, `discount_stack`). The
  migration only adds what is missing, so it is safe on every start and leaves existing data alone. The
  Python app names the columns it uses and ignores the rest: its product list, search, add, update and delete
  were run on a Rust-migrated copy of a 3,000-product database and worked.
- **Dates and stacking are Rust-only.** The Python cashier knows nothing about them. It applies a
  discount whenever the quantity reaches the minimum, on any date, and tier 2 always replaces tier 1. While
  the Python app is still the till, a discount with dates or "adds to discount 1" will not behave as the
  Rust dialog describes.
- **Search.** Search matches barcode, name, alias-group name and variant-group name as a plain,
  case-insensitive substring. `%` and `_` match themselves. Three or more characters use the trigram
  index; shorter queries use `LIKE`. Results are identical either way.

## What is built

**First run.** While no manager account exists, the setup wizard shows: business info, terminal ID,
tax and currency, manager account, printer. Nothing is written until the last page; then the business
row and settings are saved in one transaction before the manager account is created. Closing the
wizard early leaves the data folder untouched, and it shows again next launch.

**Login.** Wrong username and wrong password give the same message, with the same response time.
Hashing runs on a background thread. Enter in the username field moves to the password field; Enter in
the password field signs in. The zoom buttons save to `ui_scale_pct`. The session gate (cashiers need an
open session) is honoured.

**Supervisor and manager window.** After login, a supervisor or manager gets the areas their role
allows (Sales, Inventory, Printing, Data, Setup), and each area shows the pages the role may open. The
list is one table in `pos-core/src/nav.rs`, the same keys, order and roles as the Python tab
registry. Access is fixed by role; there are no per-user switches. Pages that are not ported yet show a
note instead of a blank screen.

**Inventory > Products.**

- **Search and paging:** 50 products per page. A slower, older search can never overwrite a newer one.
- **What the list shows:** the price and cost a product has *now*, resolved the way the Python app does
  (variant group, then alias group, then the product's own value), plus GCT and CASE tags.
- **Add, Edit, Delete:** every row has Edit and Delete buttons, a double-click anywhere else on the row also
  opens Edit, and Delete asks for confirmation.
- **Selling price:** it follows from the cost and the product group's markup,
  `price = cost x (1 + markup)`, and the dialog shows the working as you type.
  With no markup group, the price is the cost.
- **Barcode changes** ask for confirmation first. The rule is the typed `ProductWarning::BarcodeChange`
  in `pos-core`, so every front end asks the same question. A barcode already in use is refused.
- **Cases** linked to a product are repriced in the same save when its cost changes
  (using `case_profit_pct`), and the "Case Prices Updated" notice shows. A name-only edit leaves them alone.
- **Alias and variant groups:** the group is authoritative. Changing a member's cost or markup group
  updates the group, every member, and their cases, in one save. Joining a group that already has a price
  leaves the group alone and shows the group's price. Joining a new, empty group sets the group's cost and
  price from the product. A name-only edit never changes the group. The chooser lets you type to filter,
  pick "None", or create a group by name.
- **Discounts:** the dialog has a second column for them. Each product has up to two tiers. A tier is a shared
  named level (from `discount_levels`, for example 5% off 3 or more), a custom rule typed on the product
  (for example 35% off 3 or more), or none. Each tier can have optional From and Until dates
  (`YYYY-MM-DD`, inclusive), and "Discount 2 adds to discount 1" turns stacking on. A summary line shows the
  result in plain words. Choosing None really clears a tier, including its dates.
- **How a sale picks the discount** (`pos-core/src/discount.rs`, ported from the Python cashier with dates
  and stacking added): a tier counts when today is inside its dates (no dates means always) and the quantity
  reaches its minimum. When both tiers count, tier 2 replaces tier 1, or the two percentages are added if
  stacking is on (capped at 100%). The discount per unit is rounded to the cent. Checks on save: minimum
  quantity 1 to 9999, percent 0.1 to 99.9, real dates, and the start date not after the end date.
- **Not in the dialog yet:** case items (they open read-only with an explanation, and the save code refuses
  them), stock add/remove, and discounts on alias or variant groups themselves. A group's own discounts, set
  in the Python app, are not editable here and take precedence over the product's at sale time. Stock and
  group discounts are left untouched when you save a product.

## Tests

    cargo test -p pos-core -p pos-db

These cover hashing and login, setup, role navigation, money, paging, the price maths, exact cost parsing,
the barcode warning, duplicate barcodes, the case and group cascades, the discount rules (dates, override
and stacking, validation, the summary text), the discount columns round trip and their migration, and the
search index following edits and deletes.

To cross-check the Rust product list against the Python app on a real `products.db`:

    PRODUCTS_DB_FIXTURE=<products.db> PRODUCTS_QUERIES=<one search per line> PRODUCTS_OUT=<out.tsv> \
        cargo test -p pos-db python_fixture -- --ignored

This writes the ids, costs and prices of two pages per query so a script can diff them with the Python
app's `get_products`. The UI itself has no automated tests yet.

## Conventions

- Business rules go in `pos-core`, not in the UI, so every screen behaves the same and the rules are testable.
- Money is whole cents inside the Rust code (`i64`). The shared databases store dollars as `REAL`; convert
  once, where data crosses the database boundary (`money::to_cents`).
- Things the user should confirm are typed values from `pos-core` (like `ProductWarning`), and the window
  decides how to show them.
- Changes the Rust app makes to a database it shares with the Python app are additive: optional columns added
  by an idempotent migration (`EXTRA_PRODUCT_COLUMNS` in `pos-db`), never renamed or removed columns, so the
  Python app keeps working on the same file.
