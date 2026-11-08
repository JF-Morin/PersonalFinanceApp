#!/usr/bin/env bash
# Manual backup utility — creates a timestamped copy of the local finance.db.
# The app itself backs up automatically (src/backup.rs: at startup and every
# BACKUP_INTERVAL_HOURS); use this for an extra copy, e.g. before trying something risky.
#
# The database runs in WAL mode, so recent changes may still be in finance.db-wal.
# With the sqlite3 CLI installed, `.backup` writes a complete, consistent copy even
# while the app runs; without it, the -wal/-shm files are copied alongside.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
DB="$ROOT/data/finance.db"
BACKUP_DIR="$ROOT/data/db-backup"

if [ ! -f "$DB" ]; then
    echo "Backup skipped — data/finance.db not found (app not yet run locally)."
    exit 0
fi

mkdir -p "$BACKUP_DIR"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
DEST="$BACKUP_DIR/finance_$TIMESTAMP.db"

if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$DB" ".backup '$DEST'"
    echo "Database backed up → data/db-backup/finance_$TIMESTAMP.db"
else
    cp "$DB" "$DEST"
    for ext in wal shm; do
        [ -f "$DB-$ext" ] && cp "$DB-$ext" "$DEST-$ext"
    done
    echo "Database copied → data/db-backup/finance_$TIMESTAMP.db (+ -wal/-shm if present)"
    echo "Tip: install sqlite3 for a single consistent file, or stop the app before copying."
fi

# Keep only the 14 most recent backups (same default as the app's BACKUP_KEEP)
ls -t "$BACKUP_DIR"/finance_*.db 2>/dev/null | tail -n +15 | while read -r old; do
    rm -f -- "$old" "$old-wal" "$old-shm"
done
