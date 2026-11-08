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
A user picks their profile at `/login` and enters their password (Argon2id hash in `users.password_hash`). Users without a password set one on first login. The user ID is then stored in a `user_id` cookie (HTTP-only, SameSite=Lax) **signed** with a key derived from `COOKIE_SECRET` (`SignedCookieJar`; the app refuses to start if the secret is shorter than 32 bytes and warns if it is still the example value). A forged or edited cookie fails the signature check. All handlers call `handlers::get_user_id(&jar)`, which returns `Err(AppError::Unauthorized)` → redirect to `/login`.

Permissions: new users can be created without being logged in only on first run (no user yet); afterwards only by a logged-in user (settings page). A password (with confirmation) is required for every new user; a profile without one (legacy) can't log in until its password is set from the settings page — except when nobody has a password yet, where the first login sets it. The first user is the **admin** (`users.is_admin`, migration 006): only the admin can delete users, including themselves; when no admin exists (the admin deleted their account), the next user created becomes admin. Transactions can be edited/deleted only by their owner; imported files can be reparsed/deleted only by their importer.

### Live updates (SSE)
Handlers that change data call `state.events.send(AppEvent { kind, actor_id })` with `kind` = `"transactions"`, `"imports"`, `"settings"` (app preferences) or `"users"` (a user was added, edited or deleted). Every logged-in page holds an open `GET /events` connection (`hx-ext="sse"` in `layout/base.html`), and elements with `hx-trigger="sse:<kind>"` re-fetch their partial (home cards, transaction table, pending badge).

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

The `/transactions` page lists every transaction. Filters (year, search, category) and sorting are query parameters handled server-side (`TransactionFilter` / `filter_and_sort`); `/transactions/export.csv` exports the same filtered view. Edit, delete and manual entry are server-rendered modals. The "À compléter" button opens the carousel over the current user's pending/broken transactions: `GET /transactions/pending` renders one card, and each footer button (`nav` = prev / next / stay / close) posts the card to `POST /transactions/pending/:id`, which saves it and returns the next card (or an empty body to close). Shares are submitted as one `shares_json` field and validated server-side (≤ 100 %). Manual transactions in a foreign currency are converted with the exchange rate of the transaction date (`currency.rs`); the original amount is kept in `foreign_currency_amount` / `foreign_currency_id`.

### Balance calculation
The home page uses `complete` transactions only: for each pair of users and each currency it sums what each one paid on the other's behalf (via `transaction_shares`) and shows who owes whom. The stats row, balances card and chart each have their own partial endpoint (`/home/stats-row`, `/home/balances`, `/home/chart-part`) refreshed over SSE.

## Templates

`templates/` is loaded with `Tera::new("templates/**/*.html")`. Files starting with `_` are partials. A partial is its own file only when it is worth it: it is rendered on its own by a handler (HTMX fragment), reused in several places, or large (big script blocks, table rows).

- `layout/` — `shell.html` (the `<head>`, the one modal layer `#modal-root` and the one toast; extended by `base.html` and the login page), `base.html` (the app: mobile top bar, sidebar, bottom nav; `sidebarOpen` Alpine state on `<body>`), `_sidebar.html`. No layout JS: the active nav link comes from `active_nav` (passed to `base_context` by each page handler), and a `[data-fit-height]` scroll container fills the viewport via CSS (`app.css`)
- `pages/<page>/` — one folder per page: `<page>.html` plus its fragments and scripts
  - `home/` — `_balances`, `_stats_row`, `_chart_part` are rendered alone by the `/home/*` endpoints
  - `transactions/` — `_table` (rendered alone for filters/sort/refresh; always swapped whole — never single rows; re-fetches itself on `sse:transactions` / `transactions-changed`, which every write sends) → `_table_row`; `_state` (hidden sort inputs + CSV link, sent out-of-band with each `_table`); `_pending_badge`
  - `settings/` — `_users_table` (rendered alone by `GET /settings/users`; always swapped whole; re-fetches itself on SSE `users` / `users-changed`); `_preferences` (re-fetches itself on SSE `settings`)
  - `imports/` — `_table` (rendered alone; server-side sort via `?sort=&dir=`, re-fetches itself on `sse:imports` / `files-changed`)
  - `auth/` — login page; extends `shell.html` (no sidebar). Profile buttons load `modal/login.html` (verify / set password); creating a profile there is only offered on first run
- `modal/` — one file per modal, **only the card**: every modal is loaded with `hx-get=… hx-target="#modal-root" hx-swap="innerHTML"`. The overlay, z-index, mobile bottom-sheet layout and `close()` (✕/Annuler buttons, Esc, backdrop click, `close-modal` event) live once in `layout/shell.html` + `app.css`. The import modal (`GET /upload`) works the same way; its upload queue is `_import_scripts.html`, run by HTMX when the modal is swapped in. `_shares_field.html` is the shared participants input (Alpine; expects `users`, `current_user_id`, `shares_init`).
- `errors/404.html`

### HTMX conventions
- **Toast:** handlers answer HTMX requests with `handlers::toast_response` / `toast_header` → `HX-Trigger: {"toast": {"msg", "ok"}}`, shown by the toast in `shell.html`. After a plain form-post redirect, use `handlers::insert_flash` in the page handler instead (no banners). Extra events in the same header: `files-changed` / `transactions-changed` (that table re-fetches itself), `close-modal` (server-rendered modal closes); `events_header` sends events without a toast.
- **Errors:** any failed HTMX request (error status, or no answer) shows the shell's red toast automatically — with the server's message when it is short plain text (`AppError` responses are). A redirect answering an HTMX request is turned into `HX-Redirect` by the `handlers::htmx_redirects` middleware, so an expired session navigates to `/login` instead of swapping the login page into the target.
- **Validation errors inside a modal:** `handlers::htmx_inline_error("#target", msg)` swaps the message into the modal instead of the normal target.
- **Self-refreshing fragments:** an element with `hx-get` + `hx-trigger="sse:<kind>"` re-fetches itself. Also add `htmx:sseOpen from:body, visibilitychange[document.visibilityState=='visible'] from:document` so it catches up after events missed while a phone slept or the connection dropped; use `hx-disinherit="*"` on a wrapper whose `hx-target`/`hx-select` must not leak to the buttons inside.

CSS lives in `static/` (`app.css`, `components.css`, `transactions.css`, `login.css`, `404.css`). Stylesheets for pages inside the app must be linked in `layout/shell.html`, not in a page's `head` block: sidebar navigation uses `hx-boost`, which swaps only `<body>` (HTMX drops the response's `<head>`), so a page-level link would be missing after navigating. `login.css` / `404.css` are fine because those pages are always full page loads.

## Database

Writes that take more than one statement run in one DB transaction (`state.db.begin()` … `tx.commit()`), so a failure leaves nothing half-written: transaction edit/carousel save (row + shares + status), manual add, PDF import confirm, reparse, file delete (the PDF on disk is removed only after commit), preferences. Helpers used inside them take `&mut SqliteConnection` (`replace_shares`, `recalculate_status`) or any executor (`db::set_setting`). Network calls (exchange rates) happen before `begin()`, never while the DB is locked.


**Backups** (`src/backup.rs`): the app writes a consistent snapshot (`VACUUM INTO`, safe in WAL mode and while running) to `BACKUP_DIR` at startup — before migrations — and every `BACKUP_INTERVAL_HOURS`, keeping the `BACKUP_KEEP` newest `finance_YYYYMMDD_HHMMSS.db`. To restore: stop the app, replace `finance.db` with a backup (delete any `finance.db-wal`/`-shm`), start it. `scripts/backup-db.sh` makes an extra manual copy.

Migrations are in `migrations/` (`001_initial` … `006_admin`) and embedded in the binary via `sqlx::migrate!()`. Schema overview:

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
