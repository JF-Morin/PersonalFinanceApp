#!/usr/bin/env bash
# Run once after cloning: ./scripts/install-hooks.sh
# Installs git hooks from scripts/hooks/ into .git/hooks/
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
HOOKS_SRC="$ROOT/scripts/hooks"
HOOKS_DEST="$ROOT/.git/hooks"

for hook in "$HOOKS_SRC"/*; do
    name="$(basename "$hook")"
    dest="$HOOKS_DEST/$name"

    if [ -f "$dest" ] && ! grep -q "scripts/hooks" "$dest" 2>/dev/null; then
        echo "Skipping $name — a custom hook already exists at .git/hooks/$name"
        continue
    fi

    cp "$hook" "$dest"
    chmod +x "$dest"
    echo "Installed: .git/hooks/$name"
done

echo ""
echo "Git hooks installed. They will run automatically on commit and push."
