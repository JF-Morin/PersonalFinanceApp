CREATE TABLE IF NOT EXISTS banks (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT    NOT NULL,
    logo TEXT
);

CREATE TABLE IF NOT EXISTS currencies (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    name   TEXT NOT NULL,
    symbol TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS categories (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS transaction_status (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL,
    last_name TEXT,
    bank_id   INTEGER REFERENCES banks(id)
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS imported_files (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    filename     TEXT     NOT NULL,
    bank_id      INTEGER  NOT NULL REFERENCES banks(id),
    source_file  TEXT     NOT NULL,
    imported_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    row_count    INTEGER  NOT NULL DEFAULT 0,
    importer_id  INTEGER  NOT NULL REFERENCES users(id),
    -- SHA-256 hex digest of the PDF bytes; NULL for manually-entered transactions.
    content_hash TEXT     UNIQUE
);

CREATE TABLE IF NOT EXISTS transactions (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    date             TEXT    NOT NULL,
    transaction_date TEXT    NOT NULL,
    operation        TEXT    NOT NULL,
    amount           REAL    NOT NULL,
    currency_id      INTEGER NOT NULL REFERENCES currencies(id),
    category_id      INTEGER REFERENCES categories(id),
    paid_by          INTEGER NOT NULL REFERENCES users(id),
    other_share      REAL    CHECK(other_share IS NULL OR (other_share >= 0 AND other_share <= 100)),
    status_id        INTEGER NOT NULL REFERENCES transaction_status(id),
    from_file_id     INTEGER NOT NULL REFERENCES imported_files(id),
    notes            TEXT,
    -- Set by parsers when a line could not be fully parsed; cleared once the user corrects it.
    parse_note       TEXT
);

-- Seed data
INSERT OR IGNORE INTO transaction_status (id, name) VALUES
    (1, 'pending'),
    (2, 'complete'),
    (3, 'broken');   -- parser emitted a line it couldn't fully parse; needs manual correction

INSERT OR IGNORE INTO currencies (id, name, symbol)
VALUES (1, 'Euro', '€'), (2, 'US Dollar', '$'), (3, 'Canadian Dollar', 'CA$');

INSERT OR IGNORE INTO banks (id, name) VALUES (1, 'BNP Paribas'), (2, 'Crédit Mutuel');

INSERT OR IGNORE INTO categories (id, name) VALUES
    (1,  'Alimentation'),
    (2,  'Restaurants'),
    (3,  'Transport'),
    (4,  'Logement'),
    (5,  'Énergie & Utilities'),
    (6,  'Santé'),
    (7,  'Loisirs'),
    (8,  'Shopping'),
    (9,  'Voyage'),
    (10, 'Revenus'),
    (11, 'Autre');

INSERT OR IGNORE INTO settings (key, value)
VALUES ('theme', 'light'), ('default_currency_id', '1');
