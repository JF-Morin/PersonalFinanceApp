-- Foreign currency support
ALTER TABLE transactions ADD COLUMN foreign_currency_amount REAL;
ALTER TABLE transactions ADD COLUMN foreign_currency_id     INTEGER REFERENCES currencies(id);

-- ISO 4217 code for exchange-rate lookups
ALTER TABLE currencies ADD COLUMN code TEXT NOT NULL DEFAULT '';
UPDATE currencies SET code = 'EUR' WHERE name IN ('Euro', 'Euros');
UPDATE currencies SET code = 'USD' WHERE name IN ('US Dollar', 'Dollar US', 'Dollar américain');
UPDATE currencies SET code = 'CAD' WHERE name IN ('Canadian Dollar', 'Dollar canadien', 'Dollar CA');
UPDATE currencies SET code = 'GBP' WHERE name IN ('British Pound', 'Livre sterling', 'Pound');
UPDATE currencies SET code = 'CHF' WHERE name IN ('Swiss Franc', 'Franc suisse');
