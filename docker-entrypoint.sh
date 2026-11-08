#!/usr/bin/env sh
# Runs inside the container on every start.
# Database backups are made by the app itself (src/backup.rs): a consistent snapshot
# (SQLite VACUUM INTO) at startup — before migrations, so any update can be rolled back
# by restoring from BACKUP_DIR (default /app/data/db-backup) — then every
# BACKUP_INTERVAL_HOURS. (A plain `cp finance.db` used to be done here, but in WAL mode
# it can miss the latest changes still in finance.db-wal.)
set -e

exec ./personal-finance-app
