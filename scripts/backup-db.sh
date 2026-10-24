#!/usr/bin/env bash
# Manual backup utility — creates a timestamped copy of the local finance.db.
# Automatic backups on the server are handled by docker-entrypoint.sh on container start.
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

cp "$DB" "$DEST"
echo "Database backed up → data/db-backup/finance_$TIMESTAMP.db"

# Keep only the 10 most recent backups
ls -t "$BACKUP_DIR"/finance_*.db 2>/dev/null | tail -n +11 | xargs -r rm --
