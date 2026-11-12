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
| `reqwest` | 0.12 | HTTP client for exchange rates (`currency.rs`, Frankfurter API — ECB rates) when an amount isn't in the default currency |
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

# Run the tests (no server, no network, no system deps; also run by CI before the image build)
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

Permissions: new users can be created without being logged in only on first run (no user yet); afterwards only by a logged-in user (settings page). A password (with confirmation) is required for every new user; a profile without one (legacy) can't log in — except when nobody has a password yet, where the first login sets it. Each user can edit only their own profile (name — required, unique —, bank, password). The first user is the **admin** (`users.is_admin`, migration 006): only the admin removes users, including themselves. A user with data (transactions, shares, imported files) is **deactivated**, never deleted (`users.is_active`, migration 009): they can't log in, aren't listed on the login page or offered as a participant (a share they already have is kept; a new one is refused), show as "Nom (désactivé)" in existing data, and the admin can reactivate them. Only a user without data can be deleted (`GET /settings/users/:id/delete` opens the modal that offers whichever applies). The `handlers::require_active_user` middleware ends the session of a deactivated or deleted user on their next request. An admin who deactivates or deletes their own account hands the role to the oldest other active user (the last active user can't deactivate themselves); with nobody left, the next user created becomes admin. Transactions can be edited/deleted only by their owner; imported files can be reparsed/deleted only by their importer.

### Live updates (SSE)
Handlers that change data call `state.events.send(AppEvent { kind, actor_id })` with `kind` = `"transactions"`, `"imports"`, `"settings"` (app preferences) or `"users"` (a user was added, edited or deleted). Every logged-in page holds an open `GET /events` connection (`hx-ext="sse"` in `layout/base.html`), and elements with `hx-trigger="sse:<kind>"` re-fetch their partial (home cards, transaction table, pending badge).

### PDF import flow
The import modal (`modal/import.html`) queues one or more PDFs and, for each file:
1. `POST /upload/analyze` — multipart file (one, ≤ 20 MB: `DefaultBodyLimit` on the route) → PDF check (`.pdf` name, `%PDF-` / `%%EOF` in the content) → SHA-256 duplicate check → same-name check (case-insensitive; files are never renamed, a name already imported is refused) → parse on a blocking thread (`catch_unwind`, 60 s limit; pdf_extract, lopdf fallback) → `parsers::detect_and_parse()`. **A file with any broken line (`parse_note`) is rejected as a whole.** Nothing is written: the result is kept in memory (`PendingUploads` in `AppState`, 30 min) and the browser gets a one-time token.
2. `POST /upload/confirm` — the browser sends only the token (nothing else it sends is trusted) → in one DB transaction: re-check duplicates, insert the `imported_files` row and the `transactions` rows (`status = pending`), write the file to `STORAGE_PATH/Statements/<BankName>/` and commit (the file is removed if the commit fails).

If the file was already imported, the modal offers `POST /imported-files/:id/reparse`, which re-parses the stored PDF and adds only the missing transactions (matched one for one by date, operation and amount, so one of two identical lines can be restored). Imported files are listed at `/imported-files` (a filterable table: search by file name, filters in `modal/file_filters.html` — import date, bank, importer, number of transactions — sortable columns), where they can also be deleted (with or without their transactions).

### Parser interface
Each parser lives in `src/parsers/<bank>.rs` and implements `BankParser`:
- `bank_id()` / `bank_name()` — must match the bank's row in the `banks` table
- `currency_code()` — ISO code of the statements' currency (`"EUR"` for BNP Paribas and Crédit Mutuel); must exist in `currencies`
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
- `broken` — `parse_note` set: a manual transaction whose exchange rate couldn't be fetched (saved with amount 0), or a legacy imported line (imports with broken lines are now rejected). It stays broken until a non-zero amount is entered
- `complete` — category set and shares sum to exactly 100 %

The `/transactions` page lists every transaction. Search, filters and sorting (every column) are query parameters handled server-side (`TransactionFilter` / `filter_and_sort`; no `year` = current year, `year=all` = all years); the filters modal is `modal/transaction_filters.html` (period, amount, categories, owner, participants, status, source). See **Filterable tables** below. The CSV button in the page header exports the same view via `/transactions/export.csv` — plain text and numbers only (amount `-12.34`, currency ISO code, original amount, currency and exchange rate, one share column per user; the characters that would start a formula — `=`, `+`, `-`, `@` — are removed from the start of text cells). Edit, delete and manual entry are server-rendered modals. Editing an existing transaction (edit modal and carousel, both built on `modal/_transaction_fields.html`) changes only its category, shares and notes; operation and (original) amount can be corrected only on a broken line, and date/currencies/owner never change (`update_transaction` ignores anything else). The "À compléter" button opens the carousel over the current user's pending/broken transactions: `GET /transactions/pending` renders one card, and each footer button (`nav` = prev / next / stay / close) posts the card to `POST /transactions/pending/:id`, which saves it and returns the next card (or an empty body to close). Shares are submitted as one `shares_json` field and validated server-side (≤ 100 %). Manual transactions need a currency (no default). See **Money and currencies** below.

### Money and currencies
Amounts are integer cents (`*_cents` columns; `currency::to_cents` / `from_cents` / `convert`), Tricount-style (migration 007):
- `original_amount_cents` / `original_currency_id` — what really happened (the statement line in the bank's currency, or what the user typed in the currency they picked); never changed by a conversion.
- `exchange_rate` — original → `currency_id` at the transaction date (1 when the same); `NULL` = no rate found (the transaction is broken; saving it again retries the rate).
- `amount_cents` / `currency_id` — the value in the default currency, used by balances, chart, filters and sorting.

Rates come from Frankfurter (`currency::Rates::fetch`: one request per currency for the whole date range; weekends/holidays use the last working day), always before `begin()`. Every final rate (one published on or after its date) is kept in `exchange_rates` (migration 010), so a (currency pair, date) already seen is never fetched again — repeat conversions are instant and work while the service is down. A statement whose currency isn't the default is converted at import (a missing rate rejects the file); a manual entry without a rate is saved as broken. Changing the default currency (`transactions::change_default_currency`, from `POST /settings`) recomputes every `amount_cents` from the originals in one DB transaction — if any rate is missing, nothing changes (theme included) and the error is shown.

### Balance calculation
The home page uses `complete` transactions in the default currency only (the chart: spending only, negative amounts), and says which currency the amounts are in; transactions still in another currency (from before conversions existed) are counted in a notice, and saving the preferences converts them. For each pair of users it sums what each one paid on the other's behalf (via `transaction_shares`) and shows who owes whom. The stats row, balances card and chart each have their own partial endpoint (`/home/stats-row`, `/home/balances`, `/home/chart-part`) refreshed over SSE.

## Templates

`templates/` is loaded with `Tera::new("templates/**/*.html")`. Files starting with `_` are partials. A partial is its own file only when it is worth it: it is rendered on its own by a handler (HTMX fragment), reused in several places, or large (big script blocks, table rows).

- `layout/` — `shell.html` (the `<head>`, the one modal layer `#modal-root` and the one toast; extended by `base.html` and the login page), `base.html` (the app: sidebar on desktop; on phones/tablets <1024px a top bar whose avatar opens the user menu with logout, and a floating rounded bottom nav (active item on a tinted indicator, which slides between items through a view transition: `hx-swap="innerHTML transition:true"` + `view-transition-name` in `app.css`) — no sidebar drawer), `_sidebar.html`. No layout JS: the active nav link comes from `active_nav` (passed to `base_context` by each page handler), and a `[data-fit-height]` scroll container fills the viewport via CSS (`app.css`)
- `pages/<page>/` — one folder per page: `<page>.html` plus its fragments and scripts
  - `home/` — `_balances`, `_stats_row`, `_chart_part` are rendered alone by the `/home/*` endpoints
  - `transactions/` — `_table` (rendered alone for filters/sort/refresh; always swapped whole — never single rows; re-fetches itself on `sse:transactions` / `transactions-changed`, which every write sends; sends `layout/_filter_bar.html` out-of-band) → `_table_row`; `_pending_badge` (header button; its refresh also sends `_pending_strip` out-of-band). Phones (<640px): the header keeps only the title and an "Actions" button (`.phone-only`, opens the `modal/transaction_actions.html` sheet: add, import, CSV); the other buttons are `.no-phone`, and the pending count is `_pending_strip`, a full-width strip under the header
  - `settings/` — `_users_table` (rendered alone by `GET /settings/users`; always swapped whole; re-fetches itself on SSE `users` / `users-changed`); `_preferences` (re-fetches itself on SSE `settings`)
  - `imports/` — `_table` (rendered alone for search/filters/sort/refresh; re-fetches itself on `sse:imports` / `files-changed`; sends `layout/_filter_bar.html` out-of-band)
  - `auth/` — login page; extends `shell.html` (no sidebar). Profile buttons load `modal/login.html` (verify / set password); creating a profile there is only offered on first run
- **Modal pattern** (every modal looks the same — follow it for new ones): root `<div class="modal-sheet card w-full" style="--modal-width: 32rem">` (24/28/32/36rem; the width is applied by `app.css`, not Tailwind); header via `{% set modal_title/modal_icon/modal_tone/modal_subtitle %}{% include "modal/_header.html" %}` (icon tile, title, ✕); fields `.label` + `.input`; errors in `.modal-error` (hidden while empty); actions in `.modal-footer`: `btn-secondary` Annuler first, then ONE `btn-primary` (or `btn-danger` for deletions). Boxed messages use `.notice` + `.notice-error/-warning/-success`; small buttons `.btn-sm`. No native `confirm()`/`alert()` — confirmations are modals too.
- **Tables** (transactions, imported files, users) never scroll sideways: `.rtable` inside a `.rtable-wrap` container (`components.css`). Wide: a normal table, long text clamped to 2 lines. Narrow — by the table's own width (container query: 72rem for the transactions table `.rtable-wide`, 40rem for the others) — each `tr.rt-row` becomes a separate rounded card on the wrapper's tinted background, laid out as a grid: `.rt-lead` (an `.rt-tile` icon tile or `.is-avatar`, optional) | `.rt-main` (title + `.m-only` `.rt-meta` line of icon + text items, `.rt-badge` for a status) | `.rt-side` (amount) on the first line, `.rt-sub` (shares; full width when the row has no buttons) | `.rt-act` (buttons) on the second. `.rt-hide` cells disappear, `.w-only` parts show only in the wide table, `.rt-empty` is the dashed placeholder for an empty value; the column headers are hidden. Every table gets a lead tile column (source icon, PDF icon, user avatar), visible in both layouts.
- **Filterable tables** (transactions, imported files): the table card starts with a `.table-toolbar` search form — `layout/_filter_bar.html` on the left ("Filtres (n)" button opening the page's filters modal; in card mode only — the card is a `filter-card` container, `filter-card-wide` for the 72rem threshold — a `.sort-chip` showing the sort, whose label opens the modal and whose arrow flips the direction; one removable chip per active filter, "Tout effacer", and hidden inputs carrying the filters and the sort), a thin `.toolbar-search` box on the right. On phones (<640px) the table card itself disappears: the toolbar becomes its own rounded card and the item cards below take its exact width, on the page background. The table fragment is swapped alone (the search box keeps focus) and sends the filter bar out-of-band. Every request that reloads the table shares one `hx-sync` group, the table card (`#transactions-card` / `#files-card`): user actions (search, sort headers, chips, modal, scroll) use `:replace`, the table's own SSE refresh `:drop` — so an older view never lands after a newer one. Handlers drop multi-choice filter values that point to a deleted user / category / bank (`filters::retain_known`) before using the filter, so chips, count and results agree. Filters are query parameters read with axum-extra's `Query` (multi-choice filters repeat their key: `?owner=1&owner=2`; note it reads an empty value as absent); `handlers/filters.rs` has the shared parsing helpers, the chip builder and `FilterBar` (the bar's context). Filters modals ("Filtres et tri") start with the shared "Tri" section `modal/_sort_fields.html` (sort column select + direction; `data-keep`), then sections `.filters-section`, multi-choice pills `.choice-group` > `.choice`, footer Effacer (empties the filter fields only, not the sort) / Annuler / Appliquer (loads the table, then closes). Styles in `components.css`.
- `modal/` — one file per modal, **only the card**: every modal is loaded with `hx-get=… hx-target="#modal-root" hx-swap="innerHTML"`. The overlay, z-index, mobile bottom-sheet layout (swiped down to close: the small touch script under `#modal-root`) and `close()` (✕/Annuler buttons, Esc, backdrop click, `close-modal` event) live once in `layout/shell.html` + `app.css`. The import modal (`GET /upload`) works the same way; its upload queue is `_import_scripts.html`, run by HTMX when the modal is swapped in. `_shares_field.html` is the shared participants input (Alpine; expects `share_users`, `shares_init`, `split_hints` — all set by `insert_form_options`): one-tap presets (Moi 100 % / 50-50 / other 100 %, or Moi seul / Partagé également with 3+ users), "Autre…" → a slider (two people, 5 % steps, total always 100 %; the thumb sits on the side of whoever pays more) or per-person chips (equal split), "Ajuster" → exact number inputs, and a "Dernier partage" suggestion: the split of the user's latest complete transaction in the selected category, applied only on tap (kept on screen, marked "Appliqué", while the split matches it).
- `errors/404.html`

### HTMX conventions
- **Toast:** handlers answer HTMX requests with `handlers::toast_response` / `toast_header` → `HX-Trigger: {"toast": {"msg", "ok"}}`, shown by the toast in `shell.html`. After a plain form-post redirect, use `handlers::insert_flash` in the page handler instead (no banners). Extra events in the same header: `files-changed` / `transactions-changed` (that table re-fetches itself), `close-modal` (server-rendered modal closes); `events_header` sends events without a toast. HTMX fires these events on the element that made the request, so `close()` empties `#modal-root` one tick later (otherwise the toast from a modal button would be lost). The toast is styled by `.toast` in `components.css` (hidden unless `.is-shown`).
- **Errors:** any failed HTMX request (error status, or no answer) shows the shell's red toast automatically — with the server's message when it is short plain text (`AppError` responses are). A redirect answering an HTMX request is turned into `HX-Redirect` by the `handlers::htmx_redirects` middleware, so an expired session navigates to `/login` instead of swapping the login page into the target.
- **Validation errors inside a modal:** `handlers::htmx_inline_error("#target", msg)` swaps the message into the modal instead of the normal target.
- **Self-refreshing fragments:** an element with `hx-get` + `hx-trigger="sse:<kind>"` re-fetches itself. Also add `htmx:sseOpen from:body, visibilitychange[document.visibilityState=='visible'] from:document` so it catches up after events missed while a phone slept or the connection dropped; use `hx-disinherit="*"` on a wrapper whose `hx-target`/`hx-select` must not leak to the buttons inside.

CSS lives in `static/` (`app.css`, `components.css`, `transactions.css`, `login.css`, `404.css`). Stylesheets for pages inside the app must be linked in `layout/shell.html`, not in a page's `head` block: sidebar navigation uses `hx-boost`, which swaps only `<body>` (HTMX drops the response's `<head>`), so a page-level link would be missing after navigating. `login.css` / `404.css` are fine because those pages are always full page loads.

## Database

Writes that take more than one statement run in one DB transaction (`state.db.begin()` … `tx.commit()`), so a failure leaves nothing half-written: transaction edit/carousel save (row + shares + status), manual add, PDF import confirm, reparse, file delete (the PDF on disk is removed only after commit), preferences. Helpers used inside them take `&mut SqliteConnection` (`replace_shares`, `recalculate_status`) or any executor (`db::set_setting`). Network calls (exchange rates) happen before `begin()`, never while the DB is locked.


**Logs** (`src/logs.rs`): the admin sets the log level and how many days of logs to keep in Paramètres → Journaux (`POST /settings/logs`). The level applies at once (a `tracing_subscriber::reload` filter; "par défaut" = `RUST_LOG`); daily files `app.log.YYYY-MM-DD` older than that are deleted at startup, every 6 hours and on save (0 = keep all; default 30).

**Backups** (`src/backup.rs`): the app writes a consistent snapshot (`VACUUM INTO`, safe in WAL mode and while running) to `BACKUP_DIR` at startup — before migrations — and every `BACKUP_INTERVAL_HOURS`, keeping the `BACKUP_KEEP` newest `finance_YYYYMMDD_HHMMSS.db`. To restore: stop the app, replace `finance.db` with a backup (delete any `finance.db-wal`/`-shm`), start it. `scripts/backup-db.sh` makes an extra manual copy.

Migrations are in `migrations/` (`001_initial` … `011_categories`) and embedded in the binary via `sqlx::migrate!()`. sqlx runs each migration inside a DB transaction, where `PRAGMA foreign_keys = OFF` has no effect: a migration that rebuilds `transactions` must set `transaction_shares` aside first (as 007 and 008 do), or the `ON DELETE CASCADE` deletes every share. Schema overview:

- `users`, `banks`, `currencies`, `categories`, `transaction_status` — reference tables
- `settings` — key/value store for app preferences (theme, default currency) and the admin's log settings (`log_level`, `log_keep_days`)
- `exchange_rates` — rates already fetched, by (from, to, date)
- `imported_files` — one row per imported PDF
- `transactions` — one row per transaction; `from_file_id` links to `imported_files` (`NULL` = manual transaction); amounts in cents, original + converted (see **Money and currencies**)
- `transaction_shares` — per-user percentage of each transaction

## Running the app

### With Docker (recommended)

```bash
cp .env.example .env
docker compose up --build     # first build ~5-10 min; subsequent builds use cache
```

Open http://localhost:3000. On first run the login page offers to create the first profile.

Dates follow each **user's** time zone: every page stores the browser's UTC offset in a `tz_offset` cookie (`layout/shell.html`), and handlers take the `clock::UserClock` extractor for today's date, the current year, and to show UTC timestamps from the DB (`imported_at`, `last_login`) in local time. `TZ` (default `America/Toronto`; `tzdata` is in the image) is only the fallback without that cookie, and names the backups.

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

## Tests

`cargo test` runs in a few seconds, with no network: each test gets a temporary database with every migration applied (`src/test_support.rs`: `test_app()`, and builders for users, transactions, shares, known rates). The rate service is never called in tests (`currency::fetch_series` refuses): seed the rates a test needs with `seed_rate`; a missing one behaves like the service being down. The set is deliberately small, on what would fail silently:
- balances and the chart (`handlers/home.rs`);
- default-currency conversion: exact round trip, nothing changed when a rate is missing, broken rate repaired (`handlers/transactions.rs`);
- imports: broken line rejects the file, duplicates refused with nothing written, single-use personal token, reparse matching (`handlers/imports.rs`);
- migrations: an old (006) database with shares and manual entries keeps everything (`db.rs`) — extend it when a migration rebuilds a table.

In test helpers, insert with `execute` + `last_insert_rowid()`: `INSERT … RETURNING` with `fetch_one` outside a DB transaction leaves the statement open on its pooled connection and the next write fails with "database is locked".

## Adding a new bank parser

1. Create `src/parsers/<bank_slug>.rs` implementing `BankParser` (including `currency_code()`, the statements' currency)
2. Add `pub mod <bank_slug>;` in `src/parsers/mod.rs`
3. Add the bank to `all_parsers()` in `mod.rs`
4. Add the bank's seed row in a new migration (its id must match `bank_id()`)

## Naming

Write "transaction" / "transactions" in full in identifiers, ids, classes and file names — no `tx` abbreviation. (`tx` in `src/sse.rs` is the broadcast channel's sender, not a transaction.)
