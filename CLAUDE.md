# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Stack

### Rust crates

| Crate | Version | Role |
|---|---|---|
| `axum` | 0.7 | Web framework — defines routes, extracts request parts (forms, multipart, path params), and builds responses |
| `tokio` | 1 | Async runtime — drives all async I/O: HTTP connections, file writes, DB queries |
| `tokio-stream` | 0.1 | Wraps the SSE broadcast receiver as a stream (`BroadcastStream`) |
| `sqlx` | 0.7 | Async SQLite driver with a built-in migration runner (`sqlx::migrate!`) |
| `tera` | 1 | Jinja2-style server-side template engine — renders every HTML page from `templates/` |
| `serde` / `serde_json` | 1 | Serialization — struct derives for DB rows and forms, JSON responses and hidden JSON form fields |
| `tower-http` | 0.5 | HTTP middleware — request/response tracing and static file serving |
| `tracing` | 0.1 | Structured logging macros (`info!`, `warn!`, `debug!`) used throughout the codebase |
| `tracing-subscriber` | 0.3 | Wires up tracing: stdout layer (coloured, for `docker logs`) + file layer (plain text) |
| `tracing-appender` | 0.2 | Provides the daily rolling file writer used by `tracing-subscriber` |
| `pdf-extract` | 0.7 | Pure-Rust PDF text extraction — converts bank statement PDFs to plain text for the parsers |
| `lopdf` | 0.34 | Positioned text extraction (`parsers/lopdf_text.rs`) — fallback for PDFs `pdf-extract` can't read (Type3 fonts) and for parsers that use column X positions |
| `regex` | 1 | Regular expressions — used in every bank parser to match and capture transaction line fields |
| `sha2` | 0.10 | SHA-256 of uploaded PDFs, stored in `imported_files.content_hash` for duplicate detection |
| `argon2` | 0.5 | Argon2id password hashing for user passwords |
| `reqwest` | 0.12 | HTTP client for exchange rates (`currency.rs`, Frankfurter API) when a transaction is entered in a foreign currency |
| `anyhow` | 1 | Ergonomic error type for functions that can fail in multiple ways (parser internals, startup) |
| `thiserror` | 1 | Derives `std::error::Error` for the `AppError` enum without boilerplate |
| `dotenvy` | 0.15 | Loads `.env` into environment variables at startup before `Config::from_env()` runs |
| `axum-extra` | 0.9 | Extra Axum extractors — specifically `CookieJar` for reading and writing the `user_id` session cookie |
| `bytes` | 1 | Byte buffer used when reading multipart file upload data from the request stream |

### Frontend (loaded via CDN, no build step)

| Library | Version | Role |
|---|---|---|
| HTMX | 2.0.11 | AJAX from HTML attributes; partial swaps (table rows, user rows, home cards, pending badge) and `hx-boost` navigation |
| htmx-ext-sse | 2.2.4 | HTMX SSE extension — `hx-trigger="sse:<kind>"` refreshes parts of the page when `/events` pushes a change |
| Alpine.js | 3.x | Minimal declarative JS for local UI state (login page modals, settings user-edit modal) |
| Chart.js | 4 | Doughnut chart of spending per category on the home page |
| Tailwind CSS | CDN | Utility-first CSS — most styling is Tailwind classes in templates; shared rules in `static/*.css` |
| Font Awesome | 6.5.1 | Icon library |

Fonts (Inter, JetBrains Mono) come from fonts.bunny.net. The favicon is `static/icon.svg` (with `icon.png` as fallback / Apple touch icon).

### Infrastructure

| Tool | Role |
|---|---|
| SQLite | Embedded database — single file (`data/finance.db`), no server process required |
| Docker | Packages the app and its runtime into a single portable image |
| Docker Compose | Runs the container with volumes for `data/`, `storage/`, and `logs/` |

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
Browser → Axum router (`src/main.rs`) → handler (`src/handlers/`) → Tera template render → HTML response (or JSON for the fetch-based endpoints).

Unknown routes redirect to `/404-not-found` (`templates/errors/404.html`).

### State
`AppState` (cloned per request) holds the SQLite pool, config, compiled Tera instance, and the SSE `EventBus`.

### Auth
A user picks their profile at `/login` and enters their password (Argon2id hash in `users.password_hash`). Users without a password set one on first login. The user ID is then stored in a plain, unsigned `user_id` cookie — designed for a trusted home network (`COOKIE_SECRET` is read from the env but not currently used to sign anything). All handlers call `handlers::get_user_id(&jar)`, which returns `Err(AppError::Unauthorized)` → redirect to `/login`.

### Live updates (SSE)
Handlers that change data call `state.events.send(AppEvent { kind, actor_id })` with `kind` = `"transactions"`, `"imports"` or `"settings"`. Every logged-in page holds an open `GET /events` connection (`hx-ext="sse"` in `layout/base.html`), and elements with `hx-trigger="sse:<kind>"` re-fetch their partial (home cards, transaction table, pending badge).

### PDF import flow
The import modal (`modal/import.html`) queues one or more PDFs and, for each file:
1. `POST /upload/analyze` — multipart file → SHA-256 duplicate check → `spawn_blocking(pdf_extract)` (lopdf fallback) → `parsers::detect_and_parse()` → JSON with the parsed transactions.
2. `POST /upload/confirm` — the browser sends that JSON straight back → write the file to `STORAGE_PATH/Statements/<BankName>/` → insert the `imported_files` row → insert `transactions` rows (`status = pending`, or `status = broken` for lines the parser could not fully parse).

If the file was already imported, the modal offers `POST /imported-files/:id/reparse`, which re-parses the stored PDF and adds only the missing transactions. Imported files are listed at `/imported-files`, where they can also be deleted (with or without their transactions).

### Parser interface
Each parser lives in `src/parsers/<bank>.rs` and implements `BankParser`:
- `bank_id()` / `bank_name()` — must match the bank's row in the `banks` table
- `detect(&str) -> bool` — look for bank-specific keywords in extracted text
- `parse(&str) -> Result<Vec<ParsedTransaction>>` — regex-based line parsing
- `parse_positioned(&[TextItem])` — optional; parse from lopdf positioned text (return `None` to use `parse`)

Add new parsers to `src/parsers/mod.rs::all_parsers()`.

Each parser has an `#[ignore]` test called `dump_raw_text` that prints the raw extracted text for a real PDF. Drop a real statement into `test/` and run:
```bash
cargo test dump_raw_text -- --ignored --nocapture
```
Use this to verify column spacing (credit vs debit threshold) against actual PDFs.

### Transactions and shares
A transaction belongs to an owner (`owner_id`) and is split between users through the `transaction_shares` table (one row per user, `share` in percent). Status (`recalculate_status` in `handlers/transactions.rs`, called after every edit/share change):
- `pending` — imported or added, still needs a category and shares totalling 100 %
- `broken` — the parser couldn't extract every field (`parse_note` set); operation/amount must be corrected
- `complete` — category set and shares sum to exactly 100 %

The `/transactions` page lists every transaction (filters, sorting, CSV export) with edit/delete modals and a manual-entry modal. The "À compléter" button opens a carousel over the current user's pending/broken transactions (`GET /transactions/pending-json`). Manual transactions in a foreign currency are converted with the exchange rate of the transaction date (`currency.rs`); the original amount is kept in `foreign_currency_amount` / `foreign_currency_id`.

### Balance calculation
The home page uses `complete` transactions only: for each pair of users and each currency it sums what each one paid on the other's behalf (via `transaction_shares`) and shows who owes whom. The stats row, balances card and chart each have their own partial endpoint (`/home/stats-row`, `/home/balances`, `/home/chart-part`) refreshed over SSE.

## Templates

`templates/` is loaded with `Tera::new("templates/**/*.html")`. Files starting with `_` are partials. A partial is its own file only when it is worth it: it is rendered on its own by a handler (HTMX fragment), reused in several places, or large (big script blocks, table rows).

- `layout/` — `base.html` (head, mobile top bar, bottom nav), `_sidebar.html`, `_scripts.html` (active nav, sidebar toggle, `CURRENT_USER_ID`)
- `pages/<page>/` — one folder per page: `<page>.html` plus its fragments and scripts
  - `home/` — `_balances`, `_stats_row`, `_chart_part` are rendered alone by the `/home/*` endpoints
  - `transactions/` — `_table_rows` (rendered alone for HTMX refresh) → `_table_row`; `_pending_badge`; `_scripts.html` (globals, filters, table actions, sort) includes `_scripts_modals.html` and `_scripts_carousel.html` inside one IIFE — keep the include order
  - `settings/` — `_user_row` is rendered alone after a user edit; `_preferences`
  - `imports/`, `auth/` (standalone login page, does not extend `base.html`)
- `modal/` — one file per modal, markup and its script together; `modal.html` includes the right set based on what the page's context provides (`banks`, `categories`, `files`). `_shares_field.html` is the shared participants input (set `share_prefix` before including).
- `errors/404.html`

Page-specific CSS lives in `static/` (`app.css`, `components.css`, `transactions.css`, `login.css`, `404.css`).

## Database

Migrations are in `migrations/` (`001_initial` … `005_shares_refactor`) and embedded in the binary via `sqlx::migrate!()`. Schema overview:

- `users`, `banks`, `currencies`, `categories`, `transaction_status` — reference tables
- `settings` — key/value store for app preferences (theme, default currency)
- `imported_files` — one row per imported PDF (plus a synthetic `manual` row that manual transactions point to)
- `transactions` — one row per transaction; `from_file_id` links to `imported_files`
- `transaction_shares` — per-user percentage of each transaction

## Running the app

### With Docker (recommended)

```bash
cp .env.example .env
docker compose up --build     # first build ~5-10 min; subsequent builds use cache
```

Open http://localhost:3000. On first run the login page offers to create the first profile.

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
4. Add the bank's seed row in a new migration (its id must match `bank_id()`)

## Naming

Write "transaction" / "transactions" in full in identifiers, ids, classes and file names — no `tx` abbreviation. (`tx` in `src/sse.rs` is the broadcast channel's sender, not a transaction.)
