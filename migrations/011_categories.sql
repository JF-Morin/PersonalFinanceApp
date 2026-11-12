-- Migration 011: the category list.
--
-- Existing categories keep their id (transactions keep a matching category) and are
-- renamed where needed; the others are added. English → French (the app's language):
--   Housing Logement · Food Alimentation · Restaurant Restaurants · Phone Téléphone ·
--   Internet Internet · Electricity Électricité · Water supply Eau · Gaz Gaz ·
--   Transport Transport · Goods Achats · Activity Activités · Income Revenus ·
--   Misc. Divers · Health Santé · Travel Voyage
-- plus Assurances, Abonnements, Impôts et taxes, Virements (money moved between accounts
-- or people — e.g. paying someone back — so it isn't counted as another category).

UPDATE categories SET name = 'Électricité' WHERE id = 5;  -- was 'Énergie & Utilities'
UPDATE categories SET name = 'Activités'   WHERE id = 7;  -- was 'Loisirs'
UPDATE categories SET name = 'Achats'      WHERE id = 8;  -- was 'Shopping'
UPDATE categories SET name = 'Divers'      WHERE id = 11; -- was 'Autre'
-- Unchanged: 1 Alimentation, 2 Restaurants, 3 Transport, 4 Logement, 6 Santé, 9 Voyage,
-- 10 Revenus.

INSERT INTO categories (name) VALUES
    ('Téléphone'),
    ('Internet'),
    ('Eau'),
    ('Gaz'),
    ('Assurances'),
    ('Abonnements'),
    ('Impôts et taxes'),
    ('Virements');
