#!/usr/bin/env bash
# Usage:
#   check-confidential.sh --staged         (pre-commit: check staged files)
#   check-confidential.sh --push FILE...   (pre-push: check a list of files)
set -euo pipefail

MODE="${1:-}"
shift || true
ERRORS=0

BLOCKED_DIRS=("data/" "logs/" "storage/" "test/")
BLOCKED_EXTS_PATTERN="\.(env|db|sqlite|sqlite3|pdf|key|pem|p12|pfx)$"
BLOCKED_EXACT=(".env" ".env.local")
CREDENTIAL_PATTERN="(password|secret|cookie_secret|api.?key|token)\s*=\s*['\"]?[A-Za-z0-9+/]{16,}"
CREDENTIAL_ALLOW="change-this|example|your-|env::var|unwrap_or|COOKIE_SECRET.*change|#"

check_files() {
    local file
    for file in "$@"; do
        [ -z "$file" ] && continue

        # Blocked directories
        for dir in "${BLOCKED_DIRS[@]}"; do
            if [[ "$file" == "$dir"* ]]; then
                echo "  BLOCKED  $file  (inside $dir)"
                ERRORS=$((ERRORS + 1))
            fi
        done

        # Blocked extensions
        if echo "$file" | grep -qiE "$BLOCKED_EXTS_PATTERN"; then
            # .env.example is the one allowed exception
            if [[ "$file" != ".env.example" ]]; then
                echo "  BLOCKED  $file  (sensitive extension)"
                ERRORS=$((ERRORS + 1))
            fi
        fi

        # Exact blocked filenames
        for exact in "${BLOCKED_EXACT[@]}"; do
            if [[ "$file" == "$exact" || "$file" == *"/$exact" ]]; then
                echo "  BLOCKED  $file  (sensitive filename)"
                ERRORS=$((ERRORS + 1))
            fi
        done
    done
}

if [ "$MODE" = "--staged" ]; then
    echo "Checking staged files for sensitive content..."
    mapfile -t FILES < <(git diff --cached --name-only)
    check_files "${FILES[@]}"

    # Scan staged diff content for credential-like strings
    SUSPICIOUS=$(git diff --cached -U0 \
        | grep "^+" | grep -v "^+++" \
        | grep -iE "$CREDENTIAL_PATTERN" \
        | grep -ivE "$CREDENTIAL_ALLOW" || true)
    if [ -n "$SUSPICIOUS" ]; then
        echo "  WARNING  Possible hardcoded credentials in staged diff:"
        echo "$SUSPICIOUS" | head -5
        ERRORS=$((ERRORS + 1))
    fi

elif [ "$MODE" = "--push" ]; then
    echo "Checking pushed commits for sensitive files..."
    # Arguments after --push are file paths
    check_files "$@"

else
    echo "Usage: check-confidential.sh --staged | --push FILE..."
    exit 1
fi

echo ""
if [ "$ERRORS" -gt 0 ]; then
    echo "Confidentiality check FAILED — $ERRORS issue(s) found."
    echo "Run 'git rm --cached <file>' to untrack a file, then add it to .gitignore."
    exit 1
fi

echo "Confidentiality check passed."
exit 0
