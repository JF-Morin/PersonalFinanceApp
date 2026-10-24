#!/usr/bin/env bash
# Builds the Docker image locally and runs the app for manual testing.
# Mirrors the production container as closely as possible.
# Usage: ./scripts/test-local.sh [--no-build]
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
IMAGE="personal-finance-app:local-test"
CONTAINER="personal-finance-app-test"
PORT="${APP_PORT:-3001}"   # Use 3001 to avoid clashing with a running production instance

# Load .env if present
if [ -f "$ROOT/.env" ]; then
    set -a; source "$ROOT/.env"; set +a
fi

COOKIE_SECRET="${COOKIE_SECRET:-local-test-secret-not-for-production!!}"

cleanup() {
    echo ""
    echo "Stopping test container..."
    docker rm -f "$CONTAINER" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Build unless --no-build is passed
if [[ "${1:-}" != "--no-build" ]]; then
    echo "Building Docker image..."
    docker build -t "$IMAGE" "$ROOT"
fi

# Remove any leftover container from a previous run
docker rm -f "$CONTAINER" 2>/dev/null || true

echo ""
echo "Starting test container on http://localhost:$PORT"
echo "Press Ctrl+C to stop."
echo ""

docker run \
    --name "$CONTAINER" \
    --rm \
    -p "$PORT:3000" \
    -e DATABASE_URL="sqlite:/app/data/finance.db" \
    -e STORAGE_PATH="/app/storage" \
    -e LOG_PATH="/app/logs" \
    -e SERVER_HOST="0.0.0.0" \
    -e SERVER_PORT="3000" \
    -e COOKIE_SECRET="$COOKIE_SECRET" \
    -e RUST_LOG="${RUST_LOG:-personal_finance_app=debug,tower_http=info}" \
    -v "$ROOT/data:/app/data" \
    -v "$ROOT/storage:/app/storage" \
    -v "$ROOT/logs:/app/logs" \
    -v "$ROOT/templates:/app/templates" \
    "$IMAGE"
