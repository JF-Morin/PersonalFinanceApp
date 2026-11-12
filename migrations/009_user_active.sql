-- Migration 009: users are deactivated instead of deleted once they have data
-- (transactions, shares, imported files), so other users' history and balances stay
-- intact. A deactivated user can't log in and isn't offered as a participant; the admin
-- can reactivate them. Only a user without any data can still be deleted.
ALTER TABLE users ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1;
