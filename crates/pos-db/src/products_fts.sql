-- Copied from core/db_products.py (_FTS_SQL): trigram search index kept in sync by triggers.
CREATE VIRTUAL TABLE IF NOT EXISTS products_fts USING fts5(
    barcode, name, content='products', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS products_fts_ai AFTER INSERT ON products BEGIN
    INSERT INTO products_fts(rowid, barcode, name) VALUES (new.id, new.barcode, new.name);
END;
CREATE TRIGGER IF NOT EXISTS products_fts_ad AFTER DELETE ON products BEGIN
    INSERT INTO products_fts(products_fts, rowid, barcode, name)
    VALUES ('delete', old.id, old.barcode, old.name);
END;
CREATE TRIGGER IF NOT EXISTS products_fts_au AFTER UPDATE OF barcode, name ON products BEGIN
    INSERT INTO products_fts(products_fts, rowid, barcode, name)
    VALUES ('delete', old.id, old.barcode, old.name);
    INSERT INTO products_fts(rowid, barcode, name) VALUES (new.id, new.barcode, new.name);
END;
