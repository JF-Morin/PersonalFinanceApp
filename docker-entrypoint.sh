#!/usr/bin/env sh
# Runs inside the container on every start.
# Creates a timestamped database backup before launching the app,
# so any update can be rolled back by restoring from /app/data/db-backup/.
set -e

DB="/app/data/finance.db"
BACKUP_DIR="/app/data/db-backup"

if [ -f "$DB" ]; then
    mkdir -p "$BACKUP_DIR"
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    cp "$DB" "$BACKUP_DIR/finance_$TIMESTAMP.db"
    echo "DB backup created: $BACKUP_DIR/finance_$TIMESTAMP.db"

    # Keep only the 10 most recent backups to avoid filling the volume
    ls -t "$BACKUP_DIR"/finance_*.db 2>/dev/null | tail -n +11 | xargs -r rm -f
fi

exec ./personal-finance-app
