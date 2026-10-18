# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Stack

- **Backend**: Rust, Axum 0.7, Tokio, SQLx 0.7 (SQLite)
- **Frontend**: Tera templates (server-side), HTMX, Alpine.js, Tailwind CSS (all via CDN)
- **PDF parsing**: `pdf-extract` crate (pure Rust, no native deps)
- **Container**: Single Docker container; SMB share mounted as a volume for PDF storage

## Common commands

```bash
# Run locally (requires Rust toolchain + .env file)
cp .env.example .env
mkdir -p data storage
cargo run

# Build and run with Docker
docker compose up --build

# Run tests
cargo test
```

## Architecture

### Request flow
Browser → Axum router (`src/main.rs`) → handler (`src/handlers/`) → Tera template render → HTML response

### State
`AppState` (cloned per request) holds the SQLite pool, config, and compiled Tera instance.

### Auth
No passwords. A user selects their name at `/login`; the user ID is stored in a plain cookie (`user_id`). All handlers call `handlers::get_user_id(&jar)` which returns `Err(AppError::Unauthorized)` → redirect to `/login`.

### PDF import flow
1. `POST /upload` — multipart file → `spawn_blocking(pdf_extract)` → `parsers::detect_and_parse()` → write file to `STORAGE_PATH/Statements/<BankName>/` → render preview in template
2. `POST /upload/confirm` — transactions JSON (hidden form field) → insert `imported_files` row → insert `transactions` rows (all `status = pending`)

### Parser interface
Each parser lives in `src/parsers/<bank>.rs` and implements `BankParser`:
- `detect(&str) -> bool` — look for bank-specific keywords in extracted text
- `parse(&str) -> Result<Vec<ParsedTransaction>>` — regex-based line parsing

Add new parsers to `src/parsers/mod.rs::all_parsers()`.

### Transaction completion
Transactions start as `status = pending`. The `/incomplete-transactions` page lets users add `category_id` and `other_share`. When both are set and "Marquer complète" is clicked, `status` → `complete`.

### Balance calculation
Home page queries all `complete` transactions with `other_share > 0` and computes net amounts owed between users (aggregated per currency).

## Database

Migrations are in `migrations/` and embedded in the binary via `sqlx::migrate!()`. Schema overview:

- `users`, `banks`, `currencies`, `categories`, `transaction_status` — reference tables
- `settings` — key/value store for app preferences
- `imported_files` — one row per parsed PDF
- `transactions` — one row per transaction; `from_file_id` links to `imported_files`

## Running the app

### With Docker (recommended)

```bash
cp .env.example .env          # edit COOKIE_SECRET for production
docker compose up --build     # first build ~5-10 min; subsequent builds use cache
```

Open http://localhost:3000. Go to `/settings` directly to create your first user, then log in.

To change the port: set `APP_PORT=8080` in `.env`.

### Locally without Docker

Requires the [Rust toolchain](https://rustup.rs/).

```bash
cp .env.example .env
mkdir -p data storage
cargo run
```

### Deploying to unRaid

1. Copy or clone the project onto the server.
2. In `docker-compose.yml`, point the storage volume at your SMB share:
   ```yaml
   - /mnt/user/PersonalFinanceApp:/app/storage
   ```
3. `docker compose up -d --build`
4. Use the **Docker Compose Manager** plugin (or User Scripts) to auto-start on boot.

## Adding a new bank parser

1. Create `src/parsers/<bank_slug>.rs` implementing `BankParser`
2. Add `pub mod <bank_slug>;` in `src/parsers/mod.rs`
3. Add the bank to `all_parsers()` in `mod.rs`
4. Add a seed row to `migrations/001_initial.sql` (or a new migration)
