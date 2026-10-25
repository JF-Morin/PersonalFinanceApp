-- Migration 005: Replace paid_by/other_share with owner_id/transaction_shares
-- SQLite cannot DROP COLUMN when a CHECK constraint exists, so we recreate the table.

PRAGMA foreign_keys = OFF;

CREATE TABLE transactions_new (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    date                    TEXT    NOT NULL,
    transaction_date        TEXT    NOT NULL,
    operation               TEXT    NOT NULL,
    amount                  REAL    NOT NULL,
    currency_id             INTEGER NOT NULL REFERENCES currencies(id),
    category_id             INTEGER REFERENCES categories(id),
    owner_id                INTEGER NOT NULL REFERENCES users(id),
    status_id               INTEGER NOT NULL REFERENCES transaction_status(id),
    from_file_id            INTEGER NOT NULL REFERENCES imported_files(id),
    notes                   TEXT,
    parse_note              TEXT,
    foreign_currency_amount REAL,
    foreign_currency_id     INTEGER REFERENCES currencies(id)
);

INSERT INTO transactions_new
    SELECT id, date, transaction_date, operation, amount, currency_id, category_id,
           paid_by, status_id, from_file_id, notes, parse_note,
           foreign_currency_amount, foreign_currency_id
    FROM transactions;

DROP TABLE transactions;
ALTER TABLE transactions_new RENAME TO transactions;

-- Per-transaction share ownership; sum must equal 100 for a transaction to be complete
CREATE TABLE IF NOT EXISTS transaction_shares (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id INTEGER NOT NULL REFERENCES transactions(id) ON DELETE CASCADE,
    user_id        INTEGER NOT NULL REFERENCES users(id),
    share          REAL    NOT NULL CHECK(share > 0 AND share <= 100),
    UNIQUE(transaction_id, user_id)
);

-- Reset all previously-complete transactions to pending: they have no shares yet
UPDATE transactions SET status_id = 1 WHERE status_id = 2;

PRAGMA foreign_keys = ON;
