-- Migration 007: amounts as integer cents, and every transaction keeps its original
-- amount and currency (Tricount-style):
--   original_amount_cents / original_currency_id — what really happened (bank statement
--       line, or what the user typed); never changed by a currency conversion.
--   exchange_rate — original currency → `currency_id` at the transaction date (1 when they
--       are the same); NULL when no rate could be found (the transaction is then broken).
--   amount_cents / currency_id — the converted value in the default currency, used by the
--       balances and the chart; recomputed from the originals when the default changes.
--
-- sqlx runs a migration inside a DB transaction, where `PRAGMA foreign_keys = OFF` has no
-- effect, and dropping `transactions` would cascade-delete every `transaction_shares` row.
-- So the shares are set aside first and restored after the rebuild.

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
    from_file_id          INTEGER NOT NULL REFERENCES imported_files(id),
    notes                 TEXT,
    parse_note            TEXT
);

-- Before: `amount` (REAL) in `currency_id`, plus `foreign_currency_amount` /
-- `foreign_currency_id` for manual entries in another currency.
INSERT INTO transactions_new (
    id, date, transaction_date, operation,
    original_amount_cents, original_currency_id, exchange_rate,
    amount_cents, currency_id,
    category_id, owner_id, status_id, from_file_id, notes, parse_note
)
SELECT
    id, date, transaction_date, operation,
    CAST(ROUND(COALESCE(foreign_currency_amount, amount) * 100) AS INTEGER),
    COALESCE(foreign_currency_id, currency_id),
    CASE
        WHEN foreign_currency_id IS NULL OR foreign_currency_id = currency_id THEN 1.0
        -- Converted manual entry: the rate it was converted with.
        WHEN foreign_currency_amount != 0 AND amount != 0 THEN amount / foreign_currency_amount
        -- The rate couldn't be fetched (saved as broken, amount 0).
        ELSE NULL
    END,
    CAST(ROUND(amount * 100) AS INTEGER),
    currency_id,
    category_id, owner_id, status_id, from_file_id, notes, parse_note
FROM transactions;

DROP TABLE transactions;
ALTER TABLE transactions_new RENAME TO transactions;

-- Same definition as in migration 005.
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
