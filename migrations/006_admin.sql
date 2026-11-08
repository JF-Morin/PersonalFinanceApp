-- Admin role: the first user of the app is the admin. Only the admin can delete users
-- (others or themselves). When there is no admin (the admin deleted their own account),
-- the next user created becomes the admin.
ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;
UPDATE users SET is_admin = 1 WHERE id = (SELECT MIN(id) FROM users);
