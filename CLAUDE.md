# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Stack

### Rust crates

| Crate | Version | Role |
|---|---|---|
| `axum` | 0.7 | Web framework — defines routes, extracts request parts (forms, multipart, path params), and builds responses |
| `tokio` | 1 | Async runtime — drives all async I/O: HTTP connections, file writes, DB queries |
| `sqlx` | 0.7 | Async SQLite driver with compile-time query checking and a built-in migration runner (`sqlx::migrate!`) |
| `tera` | 1 | Jinja2-style server-side template engine — renders every HTML page from `templates/` |
| `serde` / `serde_json` | 1 | Serialization framework — struct derives for DB rows, JSON encoding of the transaction preview form field |
| `tower-http` | 0.5 | HTTP middleware — request/response tracing (method, path, status, latency) and static file serving |
| `tracing` | 0.1 | Structured logging macros (`info!`, `warn!`, `debug!`) used throughout the codebase |
| `tracing-subscriber` | 0.3 | Wires up tracing: stdout layer (coloured, for `docker logs`) + file layer (plain text) |
| `tracing-appender` | 0.2 | Provides the daily rolling file writer used by `tracing-subscriber` |
| `pdf-extract` | 0.7 | Pure-Rust PDF text extraction — converts bank statement PDFs to plain text for the parsers |
| `regex` | 1 | Regular expressions — used in every bank parser to match and capture transaction line fields |
| `chrono` | 0.4 | Date/time types, pulled in as a SQLx feature so timestamp columns map to typed values |
| `anyhow` | 1 | Ergonomic error type for functions that can fail in multiple ways (parser internals, startup) |
| `thiserror` | 1 | Derives `std::error::Error` for the `AppError` enum without boilerplate |
| `dotenvy` | 0.15 | Loads `.env` into environment variables at startup before `Config::from_env()` runs |
| `axum-extra` | 0.9 | Extra Axum extractors — specifically `CookieJar` for reading and writing the `user_id` session cookie |
| `bytes` | 1 | Zero-copy byte buffer used when reading multipart file upload data from the request stream |

### Frontend (loaded via CDN, no build step)

| Library | Version | Role |
|---|---|---|
| HTMX | 1.9.10 | Sends AJAX requests from HTML attributes; handles partial page updates (upload preview, card swaps on save/complete) |
| Alpine.js | 3.x | Minimal declarative JS for local UI state (collapsible transaction cards, open/close toggles) |
| Tailwind CSS | CDN | Utility-first CSS — all styling is done with Tailwind classes directly in templates |
| Font Awesome | 6.5.1 | Icon library — icons throughout the UI (chevrons, warning triangles, save/check buttons, etc.) |

### Infrastructure

| Tool | Role |
|---|---|
| SQLite | Embedded database — single file (`data/finance.db`), no server process required |
| Docker | Packages the app and its runtime into a single portable image |
| Docker Compose | Runs the container with named volumes for `data/`, `storage/`, and `logs/` |

## Common commands

```bash
# Run locally (requires Rust toolchain — https://rustup.rs)
cp .env.example .env
cargo run               # creates data/, storage/, logs/ automatically on first run

# Build and run with Docker
docker compose up --build

# Run unit tests (parser logic — no server needed, no system deps)
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
2. `POST /upload/confirm` — transactions JSON (hidden form field) → insert `imported_files` row → insert `transactions` rows (`status = pending`, or `status = broken` for lines the parser could not fully parse)

### Parser interface
Each parser lives in `src/parsers/<bank>.rs` and implements `BankParser`:
- `detect(&str) -> bool` — look for bank-specific keywords in extracted text
- `parse(&str) -> Result<Vec<ParsedTransaction>>` — regex-based line parsing

Add new parsers to `src/parsers/mod.rs::all_parsers()`.

Each parser has an `#[ignore]` test called `dump_raw_text` that prints the raw pdf_extract output for a real PDF. Drop a real statement into `test/` and run:
```bash
cargo test dump_raw_text -- --ignored --nocapture
```
Use this to verify column spacing (credit vs debit threshold) against actual PDFs.

### Transaction completion
Transactions start as `status = pending` (or `status = broken` for unrecognized parser lines). The `/incomplete-transactions` page shows both: broken transactions have editable operation/amount fields to correct the raw data; pending transactions just need `category_id` and `other_share`. When those are set and "Marquer complète" is clicked, `status` → `complete`.

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

To change the host port: set `APP_PORT=8080` in `.env` (Docker only — for local runs, change `SERVER_PORT` instead).

### Locally without Docker

Requires the [Rust toolchain](https://rustup.rs/). No other system packages needed (SQLx uses pure-Rust TLS).

```bash
cp .env.example .env
cargo run       # data/, storage/, logs/ are created automatically on first run
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
