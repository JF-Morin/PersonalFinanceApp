-- Migration 010: exchange rates already fetched, so the same conversion never asks the
-- rate service twice (a default-currency change, a reimported statement, a manual entry
-- on a date already seen) and keeps working when the service is down.
--
-- One row per (from, to, date) as used for a transaction on that date — for a weekend or
-- a holiday, the rate of the last working day before it. Only final rates are stored: a
-- rate is kept once the European Central Bank has published one for that date or a later
-- one (a date with no rate published yet could still get one). See `currency.rs`.
CREATE TABLE exchange_rates (
    from_code TEXT NOT NULL,
    to_code   TEXT NOT NULL,
    date      TEXT NOT NULL,
    rate      REAL NOT NULL CHECK (rate > 0),
    PRIMARY KEY (from_code, to_code, date)
);
