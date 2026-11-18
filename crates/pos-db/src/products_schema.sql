-- Copied from core/db_products.py (SCHEMA). Keep it identical: the Rust and Python apps share products.db.
CREATE TABLE IF NOT EXISTS groups (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL UNIQUE,
    profit_margin REAL    NOT NULL DEFAULT 0.0
);

CREATE TABLE IF NOT EXISTS discount_levels (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE,
    min_qty     INTEGER NOT NULL DEFAULT 1,
    percent     REAL    NOT NULL DEFAULT 0.0
);

CREATE TABLE IF NOT EXISTS alias_groups (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    name                  TEXT    NOT NULL UNIQUE,  -- internal label only, not a product name
    cost                  REAL    NOT NULL DEFAULT 0.0,
    selling_price         REAL    NOT NULL DEFAULT 0.0,
    discount_level1_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    discount_level2_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    inline_discount1_qty  INTEGER DEFAULT NULL,
    inline_discount1_pct  REAL    DEFAULT NULL,
    inline_discount2_qty  INTEGER DEFAULT NULL,
    inline_discount2_pct  REAL    DEFAULT NULL
);

CREATE TABLE IF NOT EXISTS variant_groups (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    name                  TEXT    NOT NULL UNIQUE,
    cost                  REAL    NOT NULL DEFAULT 0.0,
    selling_price         REAL    NOT NULL DEFAULT 0.0,
    stock                 INTEGER NOT NULL DEFAULT 0,
    discount_level1_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    discount_level2_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    inline_discount1_qty  INTEGER DEFAULT NULL,
    inline_discount1_pct  REAL    DEFAULT NULL,
    inline_discount2_qty  INTEGER DEFAULT NULL,
    inline_discount2_pct  REAL    DEFAULT NULL
);

CREATE TABLE IF NOT EXISTS products (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    barcode           TEXT    NOT NULL UNIQUE,
    name              TEXT    NOT NULL,
    group_id          INTEGER REFERENCES groups(id) ON DELETE SET NULL,

    -- Own pricing/stock — authoritative only when standalone
    -- (alias_group_id IS NULL AND variant_group_id IS NULL)
    cost              REAL    NOT NULL DEFAULT 0.0,
    selling_price     REAL    NOT NULL DEFAULT 0.0,
    stock             INTEGER NOT NULL DEFAULT 0,

    -- Membership — mutually exclusive with each other and with is_case
    alias_group_id    INTEGER REFERENCES alias_groups(id)   ON DELETE SET NULL,
    variant_group_id  INTEGER REFERENCES variant_groups(id) ON DELETE SET NULL,

    -- Own discounts — authoritative only when standalone
    discount_level1_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    discount_level2_id    INTEGER REFERENCES discount_levels(id) ON DELETE SET NULL,
    inline_discount1_qty  INTEGER DEFAULT NULL,
    inline_discount1_pct  REAL    DEFAULT NULL,
    inline_discount2_qty  INTEGER DEFAULT NULL,
    inline_discount2_pct  REAL    DEFAULT NULL,

    gct_applicable    INTEGER NOT NULL DEFAULT 1,

    -- Case product flag + link to what it's a case OF
    is_case                INTEGER NOT NULL DEFAULT 0,
    case_qty               INTEGER DEFAULT NULL,
    case_product_id        INTEGER REFERENCES products(id)       ON DELETE SET NULL,
    case_variant_group_id  INTEGER REFERENCES variant_groups(id) ON DELETE SET NULL,

    created_at        TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),
    updated_at        TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),

    -- A product is at most one of: alias-group member, variant-group member, case product
    CHECK (
        (CASE WHEN alias_group_id   IS NOT NULL THEN 1 ELSE 0 END) +
        (CASE WHEN variant_group_id IS NOT NULL THEN 1 ELSE 0 END) +
        (CASE WHEN is_case = 1                  THEN 1 ELSE 0 END) <= 1
    ),
    -- A case product is a case of a single product OR a variant group, never both
    CHECK (case_product_id IS NULL OR case_variant_group_id IS NULL)
);

CREATE TABLE IF NOT EXISTS stock_adjustments (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    product_id        INTEGER REFERENCES products(id) ON DELETE CASCADE,
    variant_group_id  INTEGER REFERENCES variant_groups(id) ON DELETE CASCADE,
    qty_change        INTEGER NOT NULL,
    reason            TEXT    NOT NULL DEFAULT 'Restock',
    adjusted_by       INTEGER DEFAULT NULL,
    adjusted_at       TEXT    NOT NULL DEFAULT (datetime('now', 'localtime')),
    CHECK ((product_id IS NULL) != (variant_group_id IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_products_barcode      ON products(barcode);
CREATE INDEX IF NOT EXISTS idx_products_name         ON products(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_products_alias_grp    ON products(alias_group_id);
CREATE INDEX IF NOT EXISTS idx_products_variant_grp  ON products(variant_group_id);
CREATE INDEX IF NOT EXISTS idx_products_case_product ON products(case_product_id);
CREATE INDEX IF NOT EXISTS idx_products_case_vg      ON products(case_variant_group_id);
CREATE INDEX IF NOT EXISTS idx_stock_adj_product     ON stock_adjustments(product_id);
CREATE INDEX IF NOT EXISTS idx_stock_adj_variant     ON stock_adjustments(variant_group_id);
