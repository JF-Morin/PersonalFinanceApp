-- Migration 008: a manual transaction has no file — `from_file_id` is NULL — instead of
-- pointing to a synthetic `imported_files` row named 'manual'. That row is removed: it
-- recorded whoever added the first manual transaction as its "importer", which made
-- that user impossible to delete.
--
-- `from_file_id` loses its NOT NULL, so the table is rebuilt. As in migration 007, the
-- shares are set aside first: inside the migration's DB transaction foreign keys stay on,
-- and dropping `transactions` would cascade-delete them.

CREATE TABLE shares_backup AS SELECT id, transaction_id, user_id, share FROM transaction_shares;
DROP TABLE transaction_shares;

CREATE TABLE transactions_new (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    date                  TEXT    NOT NULL,
    transaction_date      TEXT    NOT NULL,
    operation             TEXT    NOT NULL,
    original_amount_cents INTEGER NOT NULL,
    original_currency_id  INTEGER NOT NULL REFERENCES currencies(id),
    exchange_rate         REAL    CHECK (exchange_rate IS NULL OR exchange_rate > 0),
    amount_cents          INTEGER NOT NULL,
    currency_id           INTEGER NOT NULL REFERENCES currencies(id),
    category_id           INTEGER REFERENCES categories(id),
    owner_id              INTEGER NOT NULL REFERENCES users(id),
    status_id             INTEGER NOT NULL REFERENCES transaction_status(id),
    -- NULL = manual transaction (not from an imported PDF).
    from_file_id          INTEGER REFERENCES imported_files(id),
    notes                 TEXT,
    parse_note            TEXT
);

INSERT INTO transactions_new (
    id, date, transaction_date, operation,
    original_amount_cents, original_currency_id, exchange_rate,
    amount_cents, currency_id,
    category_id, owner_id, status_id, from_file_id, notes, parse_note
)
SELECT
    t.id, t.date, t.transaction_date, t.operation,
    t.original_amount_cents, t.original_currency_id, t.exchange_rate,
    t.amount_cents, t.currency_id,
    t.category_id, t.owner_id, t.status_id,
    CASE WHEN f.filename = 'manual' THEN NULL ELSE t.from_file_id END,
    t.notes, t.parse_note
FROM transactions t
LEFT JOIN imported_files f ON f.id = t.from_file_id;

DROP TABLE transactions;
ALTER TABLE transactions_new RENAME TO transactions;

-- Same definition as in migrations 005 and 007.
CREATE TABLE transaction_shares (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id INTEGER NOT NULL REFERENCES transactions(id) ON DELETE CASCADE,
    user_id        INTEGER NOT NULL REFERENCES users(id),
    share          REAL    NOT NULL CHECK(share > 0 AND share <= 100),
    UNIQUE(transaction_id, user_id)
);
INSERT INTO transaction_shares (id, transaction_id, user_id, share)
    SELECT id, transaction_id, user_id, share FROM shares_backup;
DROP TABLE shares_backup;

-- Nothing points to the synthetic row any more.
DELETE FROM imported_files WHERE filename = 'manual';
