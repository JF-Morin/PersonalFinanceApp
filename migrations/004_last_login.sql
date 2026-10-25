-- Track when each user last authenticated
ALTER TABLE users ADD COLUMN last_login TEXT;
