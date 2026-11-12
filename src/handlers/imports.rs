/// Import handlers — PDF upload, analysis, confirmation, deletion, and reparse.
///
/// The import flow is two-step:
///   1. POST /upload/analyze   — receive the PDF, check it (PDF only, size, name and content
///                               not already imported), extract text, run the bank parsers.
///                               Nothing is written: the result is kept in memory
///                               (`PendingUploads`) and the browser gets a one-time token.
///   2. POST /upload/confirm   — the browser sends the token back; the PDF is written to
///                               storage and the `imported_files` / `transactions` rows are
///                               inserted, all or nothing. Nothing the browser sends is
///                               trusted besides the token (which only its uploader can use).
///
/// A file is rejected as a whole — nothing saved on disk or in the DB — when any of its
/// lines can't be fully parsed (broken), when its balance check fails, when another
/// imported file has the same name, or when a file with the same content (SHA-256 of the
/// bytes, `imported_files.content_hash`) was already imported — then the modal offers a
/// reparse of that file instead.
///
/// For PDFs that use Type3 fonts (which crash pdf_extract), we fall back to lopdf-based
/// positioned text extraction where column X coordinates distinguish debits from credits.
use axum::{
    extract::{Multipart, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json},
    Form,
};
// axum-extra's Query accepts repeated keys (`?bank=1&bank=2`) into a Vec.
use axum_extra::extract::{cookie::SignedCookieJar, Query};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{
    clock::UserClock,
    currency::{self, convert, to_cents, Rates},
    error::{AppError, AppResult},
    models::{Bank, ParsedTransaction, User},
    parsers::{all_parsers, detect_and_parse, detect_and_parse_positioned, lopdf_text, ParsedStatement},
    AppState,
};
use super::{
    base_context, is_htmx, is_htmx_boost, toast_response,
    filters::{self, FilterBar},
};

/// Enriched file row joined with bank name and importer name for the imports table.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ImportedFileView {
    id: i64,
    filename: String,
    imported_at: String,
    row_count: i64,
    source_file: String,
    bank_id: i64,
    bank_name: String,
    importer_name: String,
    importer_id: i64,
}

/// Query string of the imported files page: `search` (file name) from the search box,
/// the filters from `modal/file_filters.html` (`bank` and `importer` repeat their key),
/// `sort` / `dir` from the column headers. No `year` means all years here.
#[derive(Deserialize, Default, Clone)]
pub struct FilesFilter {
    search: Option<String>,
    year: Option<String>,
    month: Option<String>,
    /// Inclusive bounds on the import date, `YYYY-MM-DD`.
    date_from: Option<String>,
    date_to: Option<String>,
    #[serde(default)]
    bank: Vec<String>,
    #[serde(default)]
    importer: Vec<String>,
    /// Bounds on the file's current number of transactions.
    count_min: Option<String>,
    count_max: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
}

impl FilesFilter {
    fn search(&self) -> Option<&str> { filters::text(&self.search) }
    fn year(&self) -> Option<&str> { filters::year(&self.year) }
    fn month(&self) -> Option<u32> { filters::month(&self.month) }
    fn date_from(&self) -> Option<&str> { filters::date(&self.date_from) }
    fn date_to(&self) -> Option<&str> { filters::date(&self.date_to) }
    fn bank_ids(&self) -> Vec<i64> { filters::ids(&self.bank) }
    fn importer_ids(&self) -> Vec<i64> { filters::ids(&self.importer) }
    fn count_min(&self) -> Option<i64> { filters::number(&self.count_min).map(|n| n as i64) }
    fn count_max(&self) -> Option<i64> { filters::number(&self.count_max).map(|n| n as i64) }

    /// Forget banks / users that no longer exist (`filters::retain_known`).
    fn retain_known(&mut self, banks: &[Bank], users: &[User]) {
        let bank_ids: Vec<i64> = banks.iter().map(|b| b.id).collect();
        let user_ids: Vec<i64> = users.iter().map(|u| u.id).collect();
        filters::retain_known(&mut self.bank, &bank_ids, &[]);
        filters::retain_known(&mut self.importer, &user_ids, &[]);
    }

    /// (column key, ORDER BY expression, direction); unknown keys fall back to the
    /// default: newest import first.
    fn sort(&self) -> (&'static str, &'static str, &'static str) {
        let (key, order_by) = FILE_COLUMNS
            .iter()
            .find(|c| Some(c.0) == self.sort.as_deref())
            .map(|c| (c.0, c.3))
            .unwrap_or(("date", "f.imported_at"));
        let dir = match self.dir.as_deref() {
            Some("asc") => "asc",
            Some("desc") => "desc",
            _ if key == "date" => "desc",
            _ => "asc",
        };
        (key, order_by, dir)
    }

    /// The modal's filters as (name, value) pairs (hidden inputs of the search form).
    fn params(&self) -> Vec<(&'static str, String)> {
        let mut p = Vec::new();
        if let Some(y) = self.year() { p.push(("year", y.to_string())); }
        if let Some(m) = self.month() { p.push(("month", m.to_string())); }
        if let Some(d) = self.date_from() { p.push(("date_from", d.to_string())); }
        if let Some(d) = self.date_to() { p.push(("date_to", d.to_string())); }
        p.extend(self.bank_ids().into_iter().map(|b| ("bank", b.to_string())));
        p.extend(self.importer_ids().into_iter().map(|u| ("importer", u.to_string())));
        if let Some(n) = self.count_min() { p.push(("count_min", n.to_string())); }
        if let Some(n) = self.count_max() { p.push(("count_max", n.to_string())); }
        p
    }

    /// Search + filters (not the sort) as a query string.
    fn filter_query(&self) -> String {
        filters::query_string(self.search(), &self.params())
    }

    /// Search + filters + sort: the URL that reproduces the current table.
    fn full_query(&self) -> String {
        let (sort, _, dir) = self.sort();
        let filter = self.filter_query();
        format!("{}{}sort={}&dir={}", filter, if filter.is_empty() { "" } else { "&" }, sort, dir)
    }

    fn filter_bar(&self, banks: &[Bank], users: &[User]) -> FilterBar {
        let mut chips = filters::Chips::new(self, Self::full_query);
        if let Some(y) = self.year() {
            chips.add("fa-calendar", format!("Année : {}", y), |f| f.year = None);
        }
        if let Some(m) = self.month() {
            chips.add("fa-calendar-day", format!("Mois : {}", filters::MONTHS[m as usize - 1]), |f| f.month = None);
        }
        if let Some(p) = filters::date_range_label(self.date_from(), self.date_to()) {
            chips.add("fa-calendar-week", p, |f| { f.date_from = None; f.date_to = None; });
        }
        let selected = self.bank_ids();
        let names: Vec<&str> = banks.iter().filter(|b| selected.contains(&b.id)).map(|b| b.name.as_str()).collect();
        if !names.is_empty() {
            chips.add("fa-building-columns", format!("Banque : {}", names.join(", ")), |f| f.bank.clear());
        }
        let selected = self.importer_ids();
        let names: Vec<String> = users.iter().filter(|u| selected.contains(&u.id)).map(User::display_name).collect();
        if !names.is_empty() {
            chips.add("fa-user", format!("Importé par : {}", names.join(", ")), |f| f.importer.clear());
        }
        let count = filters::number_range_label(
            "Transactions",
            self.count_min().map(|n| n.to_string()),
            self.count_max().map(|n| n.to_string()),
        );
        if let Some(c) = count {
            chips.add("fa-list", c, |f| { f.count_min = None; f.count_max = None; });
        }

        let cleared = FilesFilter {
            search: self.search.clone(),
            sort: self.sort.clone(),
            dir: self.dir.clone(),
            ..Default::default()
        };
        let (sort, _, dir) = self.sort();
        let flipped = FilesFilter { dir: Some(filters::flip(dir).to_string()), ..self.clone() };
        FilterBar {
            id: "files-filter-bar",
            form: "file-filters",
            list_url: "/imported-files",
            target: "#files-table",
            sync: "#files-card",
            modal_url: "/imported-files/filters",
            params: self.params(),
            sort,
            dir,
            sort_label: FILE_COLUMNS.iter().find(|c| c.0 == sort).map_or("", |c| c.1),
            flip_query: flipped.full_query(),
            chips: chips.chips,
            cleared_query: cleared.full_query(),
        }
    }
}

/// One sortable column header, as the `_table.html` template needs it.
#[derive(Serialize)]
struct SortColumn {
    key: &'static str,
    label: &'static str,
    min_width: &'static str,
    /// "asc" / "desc" when the table is sorted by this column, "" otherwise.
    dir: &'static str,
    /// Direction a click on this header switches to.
    next_dir: &'static str,
}

/// (key, header label, min width, ORDER BY expression)
const FILE_COLUMNS: [(&str, &str, &str, &str); 5] = [
    ("filename",     "Fichier",      "12rem", "f.filename COLLATE NOCASE"),
    ("bank",         "Banque",       "6rem",  "b.name COLLATE NOCASE"),
    ("date",         "Importé le",   "8rem",  "f.imported_at"),
    ("transactions", "Transactions", "6rem",  "row_count"),
    ("importer",     "Importé par",  "6rem",  "importer_name COLLATE NOCASE"),
];

async fn banks_and_users(db: &crate::db::DbPool) -> AppResult<(Vec<Bank>, Vec<User>)> {
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name").fetch_all(db).await?;
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name").fetch_all(db).await?;
    Ok((banks, users))
}

/// Render the files table (`pages/imports/_table.html`) and its filter bar into `ctx`.
/// Sorting happens in SQL, filtering afterwards in Rust (a handful of rows).
async fn insert_files_table(
    ctx: &mut tera::Context,
    db: &crate::db::DbPool,
    filter: &FilesFilter,
    current_user_id: i64,
    clock: &UserClock,
) -> AppResult<()> {
    let (all_banks, users) = banks_and_users(db).await?;
    let mut filter = filter.clone();
    filter.retain_known(&all_banks, &users);
    let (sort, order_by, dir) = filter.sort();

    // `order_by` and `dir` come from the whitelists above, never from user input.
    let sql = format!(
        r#"
        SELECT f.id, f.filename, f.imported_at,
               -- Current count: `f.row_count` is frozen at import time and goes stale when
               -- single transactions are deleted or re-added by a reparse.
               (SELECT COUNT(*) FROM transactions t WHERE t.from_file_id = f.id) AS row_count,
               f.source_file,
               f.bank_id,
               b.name AS bank_name,
               u.name || COALESCE(' ' || NULLIF(u.last_name, ''), '') AS importer_name,
               f.importer_id
        FROM imported_files f
        JOIN banks b ON f.bank_id = b.id
        JOIN users u ON f.importer_id = u.id
        ORDER BY {order_by} {dir}, f.id DESC
        "#
    );
    let mut files = sqlx::query_as::<_, ImportedFileView>(&sql).fetch_all(db).await?;
    // Stored in UTC: shown, and filtered by period, in the user's time.
    for f in &mut files {
        f.imported_at = clock.local(&f.imported_at);
    }

    if let Some(s) = filter.search() {
        let s = s.to_lowercase();
        files.retain(|f| f.filename.to_lowercase().contains(&s));
    }
    let (year, month, from, to) = (filter.year(), filter.month(), filter.date_from(), filter.date_to());
    files.retain(|f| filters::in_period(&f.imported_at, year, month, from, to));
    let banks = filter.bank_ids();
    if !banks.is_empty() {
        files.retain(|f| banks.contains(&f.bank_id));
    }
    let importers = filter.importer_ids();
    if !importers.is_empty() {
        files.retain(|f| importers.contains(&f.importer_id));
    }
    if let Some(n) = filter.count_min() {
        files.retain(|f| f.row_count >= n);
    }
    if let Some(n) = filter.count_max() {
        files.retain(|f| f.row_count <= n);
    }

    let columns: Vec<SortColumn> = FILE_COLUMNS
        .iter()
        .map(|&(key, label, min_width, _)| {
            let active = if key == sort { dir } else { "" };
            SortColumn {
                key,
                label,
                min_width,
                dir: active,
                next_dir: if active == "asc" { "desc" } else { "asc" },
            }
        })
        .collect();

    let filter_query = filter.filter_query();
    ctx.insert("files", &files);
    ctx.insert("columns", &columns);
    ctx.insert("filter_query", &filter_query);
    ctx.insert("full_query", &filter.full_query());
    ctx.insert("filter_bar", &filter.filter_bar(&all_banks, &users));
    ctx.insert("current_user_id", &current_user_id);
    Ok(())
}

/// GET /imported-files — render the imported files list page.
/// HTMX requests (search, filters, sorting, SSE refresh) get only the table fragment,
/// which also updates the filter bar out-of-band.
pub async fn get_imported_files(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    clock: UserClock,
    Query(filter): Query<FilesFilter>,
) -> AppResult<Html<String>> {
    let current_user_id = super::get_user_id(&jar)?;

    if is_htmx(&headers) && !is_htmx_boost(&headers) {
        let mut ctx = tera::Context::new();
        insert_files_table(&mut ctx, &state.db, &filter, current_user_id, &clock).await?;
        ctx.insert("oob", &true);
        let html = state.tera.render("pages/imports/_table.html", &ctx)?;
        return Ok(Html(html));
    }

    let mut ctx = base_context(&jar, &state.db, "/imported-files").await?;
    insert_files_table(&mut ctx, &state.db, &filter, current_user_id, &clock).await?;
    ctx.insert("oob", &false);
    ctx.insert("search", &filter.search().unwrap_or(""));
    let html = state.tera.render("pages/imports/imports.html", &ctx)?;
    Ok(Html(html))
}

/// GET /imported-files/filters — the filters modal, pre-filled with the current view
/// (the "Filtres" button sends the search form: search, filters and sort).
pub async fn get_file_filters_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    clock: UserClock,
    Query(mut filter): Query<FilesFilter>,
) -> AppResult<Html<String>> {
    super::get_user_id(&jar)?;
    let (banks, users) = banks_and_users(&state.db).await?;
    filter.retain_known(&banks, &users);
    // Years of the import dates in the user's time (stored in UTC).
    let imported: Vec<String> = sqlx::query_scalar(
        "SELECT imported_at FROM imported_files",
    )
    .fetch_all(&state.db)
    .await?;
    let mut years: Vec<String> = imported.iter().map(|t| clock.local(t).chars().take(4).collect()).collect();
    years.sort_unstable_by(|a, b| b.cmp(a));
    years.dedup();
    let (sort, _, dir) = filter.sort();

    let mut ctx = tera::Context::new();
    ctx.insert("banks", &banks);
    ctx.insert("users", &users);
    ctx.insert("years", &years);
    ctx.insert("months", &filters::MONTHS);
    ctx.insert("search", &filter.search().unwrap_or(""));
    ctx.insert("sort", sort);
    ctx.insert("dir", dir);
    let sort_choices: Vec<filters::SortChoice> =
        FILE_COLUMNS.iter().map(|&(key, label, _, _)| filters::SortChoice { key, label }).collect();
    ctx.insert("sort_choices", &sort_choices);
    ctx.insert("year", &filter.year().unwrap_or(""));
    ctx.insert("month", &filter.month());
    ctx.insert("date_from", &filter.date_from().unwrap_or(""));
    ctx.insert("date_to", &filter.date_to().unwrap_or(""));
    ctx.insert("bank_ids", &filter.bank_ids());
    ctx.insert("importer_ids", &filter.importer_ids());
    ctx.insert("count_min", &filter.count_min().map(|n| n.to_string()).unwrap_or_default());
    ctx.insert("count_max", &filter.count_max().map(|n| n.to_string()).unwrap_or_default());

    let html = state.tera.render("modal/file_filters.html", &ctx)?;
    Ok(Html(html))
}

/// GET /upload — the import modal (loaded into `#modal-root` by the "Importer" buttons).
pub async fn get_import_modal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Html<String>> {
    super::get_user_id(&jar)?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let mut ctx = tera::Context::new();
    ctx.insert("banks", &banks);
    Ok(Html(state.tera.render("modal/import.html", &ctx)?))
}

/// File summary shown in the delete / reparse confirmation modals.
#[derive(Serialize, sqlx::FromRow)]
struct FileModalView {
    id: i64,
    filename: String,
    row_count: i64,
    importer_id: i64,
}

async fn render_file_modal(
    state: &AppState,
    jar: &SignedCookieJar,
    file_id: i64,
    template: &str,
) -> AppResult<Html<String>> {
    let user_id = super::get_user_id(jar)?;
    let file = sqlx::query_as::<_, FileModalView>(
        // Count the file's transactions now — `imported_files.row_count` is the count at
        // import time and changes when single transactions are deleted or re-added.
        "SELECT id, filename, importer_id, \
                (SELECT COUNT(*) FROM transactions WHERE from_file_id = f.id) AS row_count \
         FROM imported_files f WHERE id = ?",
    )
    .bind(file_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::Other("Fichier introuvable".into()))?;
    // Delete and reparse are reserved to the user who imported the file.
    if file.importer_id != user_id {
        return Err(AppError::Other("Vous n'êtes pas autorisé à modifier ce fichier.".into()));
    }
    let mut ctx = tera::Context::new();
    ctx.insert("file", &file);
    Ok(Html(state.tera.render(template, &ctx)?))
}

/// GET /imported-files/:id/delete — the delete confirmation modal (loaded into `#modal-root`).
pub async fn get_delete_file_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(file_id): Path<i64>,
) -> AppResult<Html<String>> {
    render_file_modal(&state, &jar, file_id, "modal/file_delete.html").await
}

/// GET /imported-files/:id/reparse — the reparse confirmation modal (loaded into `#modal-root`).
pub async fn get_reparse_file_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(file_id): Path<i64>,
) -> AppResult<Html<String>> {
    render_file_modal(&state, &jar, file_id, "modal/file_reparse.html").await
}

// ── Pending uploads (analyzed, not confirmed yet) ──────────────────────────────

/// Largest PDF accepted by `/upload/analyze` (the route's body limit, see `main.rs`).
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// How long an analyzed upload waits for its confirmation.
const PENDING_TTL: Duration = Duration::from_secs(30 * 60);
/// At most this many analyzed uploads are kept in memory (the oldest are dropped).
const PENDING_MAX: usize = 20;
/// Longest file name accepted.
const MAX_FILENAME_CHARS: usize = 150;
/// Parsing a PDF longer than this is abandoned (a malformed or hostile file).
const PARSE_TIMEOUT: Duration = Duration::from_secs(60);

/// A PDF that passed `/upload/analyze`, waiting for `/upload/confirm`. Everything the
/// confirmation writes comes from here, never from the browser.
struct PendingUpload {
    user_id: i64,
    bank_id: i64,
    bank_name: String,
    /// ISO code of the statement's currency (from the parser).
    currency_code: String,
    filename: String,
    content_hash: String,
    bytes: bytes::Bytes,
    transactions: Vec<ParsedTransaction>,
    created: Instant,
}

/// Analyzed uploads by one-time token (in `AppState`; lost on restart, which only means
/// re-analyzing the file).
#[derive(Default)]
pub struct PendingUploads(Mutex<HashMap<String, PendingUpload>>);

impl PendingUploads {
    fn insert(&self, upload: PendingUpload) -> String {
        use argon2::password_hash::rand_core::{OsRng, RngCore};
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let token: String = raw.iter().map(|b| format!("{:02x}", b)).collect();

        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, u| u.created.elapsed() < PENDING_TTL);
        while map.len() >= PENDING_MAX {
            let oldest = map.iter().min_by_key(|(_, u)| u.created).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => { map.remove(&k); }
                None => break,
            }
        }
        map.insert(token.clone(), upload);
        token
    }

    /// Take the upload out (a token works once), if it belongs to `user_id` and hasn't expired.
    fn take(&self, token: &str, user_id: i64) -> Option<PendingUpload> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(token) {
            Some(u) if u.user_id == user_id && u.created.elapsed() < PENDING_TTL => map.remove(token),
            _ => None,
        }
    }
}

/// Form submitted by the import modal once the analysis succeeded.
#[derive(Deserialize)]
pub struct ConfirmImportForm {
    /// Returned by `/upload/analyze`.
    pub token: String,
}

/// POST /upload/confirm — write the analyzed PDF to storage and its transactions to the DB.
pub async fn post_confirm_import(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<ConfirmImportForm>,
) -> impl IntoResponse {
    match confirm_import_inner(state, jar, form).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn confirm_import_inner(
    state: AppState,
    jar: SignedCookieJar,
    form: ConfirmImportForm,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;
    let upload = state.uploads.take(&form.token, user_id).ok_or_else(|| {
        AppError::Other("Analyse expirée ou introuvable : relancez l'analyse du fichier.".into())
    })?;

    // Amounts are stored in the statement's currency and converted to the default one —
    // rates fetched before the DB transaction; a missing rate rejects the whole file.
    let converted = convert_statement(&state, &upload.currency_code, &upload.transactions)
        .await?
        .map_err(AppError::Other)?;
    // One DB transaction: the file row and all its transactions, or nothing. The checks
    // run again inside it, in case the same file was imported since the analysis.
    let mut tx = state.db.begin().await?;
    if let Some(msg) = duplicate_error(&mut tx, &upload.filename, &upload.content_hash).await? {
        return Err(AppError::Other(msg));
    }

    let relative_path = format!("Statements/{}/{}", upload.bank_name, upload.filename);
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id, content_hash) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&upload.filename)
    .bind(upload.bank_id)
    .bind(&relative_path)
    .bind(upload.transactions.len() as i64)
    .bind(user_id)
    .bind(&upload.content_hash)
    .fetch_one(&mut *tx)
    .await?;

    for (transaction, amounts) in upload.transactions.iter().zip(&converted.amounts) {
        // status_id 1 = pending (broken lines never get here: the analysis rejects the file).
        insert_imported_transaction(&mut tx, transaction, amounts, &converted, user_id, file_id).await?;
    }

    // Write the PDF before committing, and remove it if the commit fails: either both the
    // file and the rows are saved, or neither. No imported file uses this name (checked
    // above), so a file already there is a leftover nothing points to.
    let statements_dir = PathBuf::from(&state.config.storage_path).join("Statements").join(&upload.bank_name);
    tokio::fs::create_dir_all(&statements_dir).await?;
    let dest_path = statements_dir.join(&upload.filename);
    tokio::fs::write(&dest_path, &upload.bytes).await?;
    if let Err(e) = tx.commit().await {
        let _ = tokio::fs::remove_file(&dest_path).await;
        return Err(e.into());
    }

    // Notify imports listeners (imports page) and transactions listeners (transactions page,
    // home page) — this operation writes rows to both tables.
    state.events.send(crate::sse::AppEvent { kind: "imports",      actor_id: user_id });
    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(
        file_id      = %file_id,
        filename     = %upload.filename,
        bank_id      = %upload.bank_id,
        transactions = %upload.transactions.len(),
        user_id      = %user_id,
        "Import confirmed and saved"
    );

    Ok(Json(serde_json::json!({ "ok": true, "file_id": file_id, "transaction_count": upload.transactions.len() }))
        .into_response())
}

/// A statement's amounts in cents, original and converted to the default currency.
struct ConvertedStatement {
    original_currency_id: i64,
    default_currency_id: i64,
    /// Per transaction, in order: (original cents, rate, converted cents).
    amounts: Vec<(i64, f64, i64)>,
}

/// Convert a statement's amounts to the default currency (rate of each line's date; one
/// request for the whole statement when its currency isn't the default). `Err` is the
/// user-facing reason the file is refused: unknown currency or missing rate.
async fn convert_statement(
    state: &AppState,
    currency_code: &str,
    transactions: &[ParsedTransaction],
) -> AppResult<Result<ConvertedStatement, String>> {
    let Some(original_currency_id) = currency::currency_id_by_code(&state.db, currency_code).await? else {
        return Ok(Err(format!("Devise du relevé inconnue de l'application ({}) : importation refusée.", currency_code)));
    };
    let default_currency_id = currency::default_currency_id(&state.db).await;
    let needs: Vec<(i64, String)> = transactions.iter().map(|t| (original_currency_id, t.date.clone())).collect();
    let rates = match Rates::fetch(&state.db, default_currency_id, &needs).await {
        Ok(rates) => rates,
        Err(e) => {
            tracing::warn!(err = %e, "Import refused: exchange rates unavailable");
            return Ok(Err(format!("Taux de change introuvables ({}) : importation refusée, réessayez plus tard.", e)));
        }
    };
    let mut amounts = Vec::with_capacity(transactions.len());
    for t in transactions {
        let Some(rate) = rates.rate(original_currency_id, &t.date) else {
            return Ok(Err(format!("Taux de change du {} introuvable : importation refusée.", t.date)));
        };
        let original = to_cents(t.amount);
        amounts.push((original, rate, convert(original, rate)));
    }
    Ok(Ok(ConvertedStatement { original_currency_id, default_currency_id, amounts }))
}

/// Insert one imported line (status pending) on the caller's DB transaction.
async fn insert_imported_transaction(
    conn: &mut sqlx::SqliteConnection,
    transaction: &ParsedTransaction,
    &(original_cents, rate, amount_cents): &(i64, f64, i64),
    converted: &ConvertedStatement,
    user_id: i64,
    file_id: i64,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO transactions (date, transaction_date, operation, original_amount_cents, \
         original_currency_id, exchange_rate, amount_cents, currency_id, owner_id, status_id, from_file_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?)",
    )
    .bind(&transaction.date)
    .bind(&transaction.transaction_date)
    .bind(&transaction.operation)
    .bind(original_cents)
    .bind(converted.original_currency_id)
    .bind(rate)
    .bind(amount_cents)
    .bind(converted.default_currency_id)
    .bind(user_id)
    .bind(file_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Why an upload can't be imported because of an existing file: same name (any case —
/// the storage share may not tell them apart) or same content. `None` = no conflict.
async fn duplicate_error(
    conn: &mut sqlx::SqliteConnection,
    filename: &str,
    content_hash: &str,
) -> AppResult<Option<String>> {
    let same_content: Option<String> = sqlx::query_scalar(
        "SELECT filename FROM imported_files WHERE content_hash = ? LIMIT 1",
    )
    .bind(content_hash)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(existing) = same_content {
        return Ok(Some(format!("Ce relevé a déjà été importé (contenu identique à « {} »).", existing)));
    }
    let same_name: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM imported_files WHERE LOWER(filename) = LOWER(?))",
    )
    .bind(filename)
    .fetch_one(&mut *conn)
    .await?;
    if same_name {
        return Ok(Some(format!(
            "Un fichier nommé « {} » a déjà été importé. Les fichiers ne sont jamais renommés : \
             vérifiez qu'il ne s'agit pas du même relevé, puis renommez-le avant de l'importer.",
            filename
        )));
    }
    Ok(None)
}

/// Check that an upload is a PDF with an acceptable name; returns the name it is stored
/// under (characters other than letters, digits, `.`, `-`, `_` become `_`).
fn check_pdf_upload(original_name: &str, bytes: &[u8]) -> Result<String, String> {
    // Only the last path component: some browsers send a full path.
    let base = original_name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let name = sanitize_filename(base);
    if !name.to_lowercase().ends_with(".pdf") || name.len() <= 4 || name.starts_with('.') {
        return Err("Seuls les fichiers PDF (.pdf) peuvent être importés.".into());
    }
    if name.chars().count() > MAX_FILENAME_CHARS {
        return Err(format!("Nom de fichier trop long (maximum {} caractères).", MAX_FILENAME_CHARS));
    }
    if bytes.is_empty() {
        return Err("Le fichier est vide.".into());
    }
    // The real type is in the content, not the name: a PDF starts with `%PDF-` (the spec
    // allows a few bytes before it) and ends with `%%EOF` (possibly followed by whitespace).
    let head = &bytes[..bytes.len().min(1024)];
    let tail = &bytes[bytes.len().saturating_sub(1024)..];
    let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    if !contains(head, b"%PDF-") || !contains(tail, b"%%EOF") {
        return Err("Ce fichier n'est pas un PDF valide.".into());
    }
    Ok(name)
}

/// POST /upload/analyze — receive a PDF, parse it, return a JSON preview (no DB writes).
pub async fn post_analyze(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    clock: UserClock,
    multipart: Multipart,
) -> impl IntoResponse {
    match analyze_inner(state, jar, clock, multipart).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn analyze_inner(
    state: AppState,
    jar: SignedCookieJar,
    clock: UserClock,
    mut multipart: Multipart,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    // Exactly one "file" field; anything else in the form is ignored.
    // A body over the route's limit fails while it is read.
    let read_error = |e: axum::extract::multipart::MultipartError| {
        tracing::warn!(error = %e, "Upload refused: unreadable multipart body");
        AppError::Other(format!(
            "Envoi du fichier impossible (taille maximale : {} Mo).",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ))
    };
    let mut upload: Option<(String, bytes::Bytes)> = None;
    while let Some(field) = multipart.next_field().await.map_err(read_error)? {
        if field.name() != Some("file") {
            continue;
        }
        if upload.is_some() {
            return Err(AppError::Other("Un seul fichier par analyse.".into()));
        }
        let name = field.file_name().unwrap_or("").to_string();
        let data = field.bytes().await.map_err(read_error)?;
        upload = Some((name, data));
    }
    let (original_name, bytes) = upload.ok_or_else(|| AppError::Other("Aucun fichier reçu.".into()))?;

    tracing::info!(filename = %original_name, bytes = %bytes.len(), "Analyze request received");

    let filename = check_pdf_upload(&original_name, &bytes).map_err(|msg| {
        tracing::warn!(filename = %original_name, reason = %msg, "Upload refused");
        AppError::Other(msg)
    })?;

    // Same content already imported → offer a reparse of that file. Checked before parsing.
    let content_hash = format!("{:x}", Sha256::digest(&bytes));
    let existing_import: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, imported_at FROM imported_files WHERE content_hash = ? LIMIT 1",
    )
    .bind(&content_hash)
    .fetch_optional(&state.db)
    .await?;
    if let Some((existing_file_id, imported_at)) = existing_import {
        tracing::info!(
            filename         = %filename,
            imported_at      = %imported_at,
            existing_file_id = %existing_file_id,
            "Duplicate content hash — offering reparse"
        );
        return Ok(Json(serde_json::json!({
            "ok": false,
            "duplicate": true,
            "existing_file_id": existing_file_id,
            "imported_at": clock.local(&imported_at),
        })).into_response());
    }
    // Same name, different content → refused (files are never renamed).
    {
        let mut conn = state.db.acquire().await?;
        if let Some(msg) = duplicate_error(&mut conn, &filename, &content_hash).await? {
            tracing::info!(filename = %filename, "Upload refused: name already imported");
            return Err(AppError::Other(msg));
        }
    }

    let ParsedStatement { bank_id, bank_name, currency_code, transactions } =
        safe_detect_and_parse(bytes.clone(), &filename).await?;

    // Bank check: if the user has a bank linked, the PDF must come from that same bank.
    let user_bank: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT u.bank_id, b.name FROM users u LEFT JOIN banks b ON u.bank_id = b.id WHERE u.id = ?",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some((Some(user_bank_id), ref user_bank_name)) = user_bank {
        if user_bank_id != bank_id {
            let linked = user_bank_name.as_deref().unwrap_or("votre banque");
            tracing::warn!(
                user_id     = %user_id,
                pdf_bank    = %bank_name,
                user_bank   = %linked,
                "Rejected PDF: bank mismatch"
            );
            return Err(AppError::Other(format!(
                "Ce fichier provient de {} mais votre compte est associé à {}. Vous ne pouvez importer que vos propres relevés de banque.",
                bank_name, linked
            )));
        }
    }

    let transaction_count = transactions.len();
    tracing::info!(
        filename          = %filename,
        bank              = %bank_name,
        transaction_count = %transaction_count,
        "Analysis complete"
    );

    // Nothing is written yet: `/upload/confirm` saves it with this token.
    let token = state.uploads.insert(PendingUpload {
        user_id,
        bank_id,
        bank_name: bank_name.clone(),
        currency_code,
        filename: filename.clone(),
        content_hash,
        bytes,
        transactions,
        created: Instant::now(),
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "token": token,
        "bank_name": bank_name,
        "transaction_count": transaction_count,
        "filename": filename,
    })).into_response())
}

/// Extract text from a PDF, run bank detection + parsing, and reject the file when any
/// line couldn't be fully parsed (a broken line rejects the whole file).
///
/// Runs on a blocking thread (CPU-bound), with `catch_unwind` so a PDF that makes a
/// library panic only fails this request, and a time limit for malformed files.
async fn safe_detect_and_parse(
    bytes: bytes::Bytes,
    filename: &str,
) -> AppResult<ParsedStatement> {
    let name = filename.to_string();
    let task = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| detect_and_parse_blocking(&bytes, &name)))
    });
    let statement = match tokio::time::timeout(PARSE_TIMEOUT, task).await {
        Err(_) => {
            tracing::warn!(filename = %filename, "PDF parsing timed out");
            return Err(AppError::Other("L'analyse du PDF a pris trop de temps : fichier refusé.".into()));
        }
        Ok(Err(e)) => return Err(AppError::Other(format!("spawn_blocking join error: {}", e))),
        Ok(Ok(Err(_panic))) => {
            tracing::warn!(filename = %filename, "PDF parsing panicked");
            return Err(AppError::Other("Ce PDF n'a pas pu être lu : fichier refusé.".into()));
        }
        Ok(Ok(Ok(result))) => result?,
    };

    if let Err(msg) = reject_broken(&statement.transactions) {
        tracing::warn!(filename = %filename, "File rejected: broken lines");
        return Err(AppError::Other(msg));
    }
    Ok(statement)
}

/// A statement with any line the parser couldn't fully read is refused as a whole; `Err`
/// lists (up to 5 of) those lines for the user.
fn reject_broken(transactions: &[ParsedTransaction]) -> Result<(), String> {
    let broken: Vec<&ParsedTransaction> = transactions.iter().filter(|t| t.parse_note.is_some()).collect();
    if broken.is_empty() {
        return Ok(());
    }
    let mut lines: Vec<String> = broken
        .iter()
        .take(5)
        .map(|t| format!("• {} — {} ({})", t.date, t.operation, t.parse_note.as_deref().unwrap_or("")))
        .collect();
    if broken.len() > 5 {
        lines.push(format!("• … et {} autre(s)", broken.len() - 5));
    }
    Err(format!(
        "{} ligne(s) n'ont pas pu être lues entièrement : le fichier est refusé.\n{}",
        broken.len(),
        lines.join("\n")
    ))
}

/// The parsing pipeline itself (blocking). Tries pdf_extract first; if it fails or panics
/// (e.g. Type3 fonts), or the matched parser prefers it, uses lopdf positioned text.
fn detect_and_parse_blocking(bytes: &[u8], filename: &str) -> AppResult<ParsedStatement> {
    let text_result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match text_result {
        Ok(Ok(text)) => {
            // If the matched parser prefers positioned extraction (e.g. BNP Paribas), use
            // lopdf so that debit/credit can be distinguished by column X coordinate rather
            // than by operation keywords.
            let prefers_positioned = all_parsers()
                .iter()
                .any(|p| p.detect(&text) && p.prefers_positioned());

            if prefers_positioned {
                tracing::info!(filename = %filename, "Parser prefers positioned — using lopdf path");
                match lopdf_fallback(bytes, filename) {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        tracing::warn!(
                            filename = %filename,
                            error    = %e,
                            "lopdf positioned failed, falling back to text parse"
                        );
                        // fall through to the text-based parse below
                    }
                }
            }

            detect_and_parse(&text).map_err(|e| {
                tracing::warn!(filename = %filename, error = %e, "No parser matched (text path)");
                AppError::Other(e.to_string())
            })
        }
        Ok(Err(pdf_err)) => {
            // pdf_extract returned an error (not a panic) — try lopdf.
            tracing::warn!(
                filename = %filename,
                error    = %pdf_err,
                "pdf_extract error, trying lopdf fallback"
            );
            lopdf_fallback(bytes, filename)
        }
        Err(_panic_payload) => {
            // pdf_extract panicked — try lopdf.
            tracing::warn!(filename = %filename, "pdf_extract panicked, trying lopdf fallback");
            lopdf_fallback(bytes, filename)
        }
    }
}

/// lopdf-based extraction path: returns positioned text items (char, x, y per page)
/// so parsers can determine debit/credit by X column position.
fn lopdf_fallback(
    bytes: &[u8],
    filename: &str,
) -> AppResult<ParsedStatement> {
    let items = lopdf_text::extract_positioned(bytes)
        .map_err(|e| AppError::Other(format!("lopdf extraction failed: {}", e)))?;

    tracing::info!(filename = %filename, items = %items.len(), "lopdf extracted positioned items");

    detect_and_parse_positioned(&items).map_err(|e| {
        tracing::warn!(filename = %filename, error = %e, "No parser matched (lopdf path)");
        AppError::Other(e.to_string())
    })
}

// ── Delete imported file ──────────────────────────────────────────────────────

/// POST /imported-files/:id/delete — delete a file together with its transactions.
/// Every imported transaction belongs to its file (a transaction is either manual or
/// from a PDF), so they always go together; a single transaction can still be deleted
/// on its own and brought back with "Réanalyser".
/// HTMX callers get a toast plus a `files-changed` event (the table re-fetches itself);
/// other callers get JSON.
pub async fn post_delete_imported_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(file_id): Path<i64>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let htmx = is_htmx(&headers);
    match delete_file_inner(state, jar, file_id, htmx).await {
        Ok(resp) => resp,
        Err(e) if htmx => toast_response(&e.user_message(), false, &["files-changed"]),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn delete_file_inner(
    state: AppState,
    jar: SignedCookieJar,
    file_id: i64,
    htmx: bool,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    // Fetch file record
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT filename, source_file, importer_id FROM imported_files WHERE id = ?",
    )
    .bind(file_id)
    .fetch_optional(&state.db)
    .await?;

    let (filename, source_file, importer_id) =
        row.ok_or_else(|| AppError::Other("Fichier introuvable".into()))?;
    // Only the user who imported a file may delete it (and with it, its transactions).
    if importer_id != user_id {
        return Err(AppError::Other("Vous n'êtes pas autorisé à supprimer ce fichier.".into()));
    }

    // One DB transaction: the file's transactions (they reference it; their shares
    // cascade) and the file row go together, or nothing is deleted.
    let mut tx = state.db.begin().await?;
    let transactions_deleted = sqlx::query("DELETE FROM transactions WHERE from_file_id = ?")
        .bind(file_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM imported_files WHERE id = ?")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Only once the DB is committed: delete the physical file (best-effort — don't fail if missing)
    let Some(physical_path) = stored_file_path(&state, &source_file) else {
        tracing::warn!(source_file = %source_file, "Stored path outside storage — PDF not deleted");
        return Ok(file_deleted_response(htmx, transactions_deleted));
    };
    match tokio::fs::remove_file(&physical_path).await {
        Ok(_) => tracing::info!(path = %physical_path.display(), "Physical PDF deleted"),
        Err(e) => tracing::warn!(
            path  = %physical_path.display(),
            error = %e,
            "Could not delete physical PDF (already missing?)"
        ),
    }

    state.events.send(crate::sse::AppEvent { kind: "imports", actor_id: user_id });
    // Also notify the transactions page when rows were actually deleted.
    if transactions_deleted > 0 {
        state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    }
    tracing::info!(
        file_id              = %file_id,
        filename             = %filename,
        transactions_deleted = %transactions_deleted,
        "Import file deleted"
    );

    Ok(file_deleted_response(htmx, transactions_deleted))
}

fn file_deleted_response(htmx: bool, transactions_deleted: u64) -> axum::response::Response {
    if htmx {
        let msg = format!("Fichier supprimé — {} transaction(s) supprimée(s).", transactions_deleted);
        toast_response(&msg, true, &["files-changed"])
    } else {
        Json(serde_json::json!({
            "ok": true,
            "transactions_deleted": transactions_deleted,
        }))
        .into_response()
    }
}

/// `STORAGE_PATH/<source_file>`, or `None` when `source_file` could point outside the
/// storage folder (`..`, an absolute path) — never read or delete such a path.
fn stored_file_path(state: &AppState, source_file: &str) -> Option<PathBuf> {
    use std::path::Component;
    let relative = std::path::Path::new(source_file);
    let safe = !source_file.is_empty() && relative.components().all(|c| matches!(c, Component::Normal(_)));
    safe.then(|| PathBuf::from(&state.config.storage_path).join(relative))
}

// ── Reparse imported file ─────────────────────────────────────────────────────

/// POST /imported-files/:id/reparse — re-run the parser and add any transactions that
/// weren't present in the original import (e.g., because the parser was improved later).
/// Existing transactions are matched by (date, operation, amount), one for one: a line that
/// appears twice in the PDF but once in the DB is added once.
pub async fn post_reparse_imported_file(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(file_id): Path<i64>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let htmx = is_htmx(&headers);
    match reparse_inner(&state, &jar, file_id, htmx).await {
        Ok(resp) => resp,
        Err(e) if htmx => toast_response(&e.user_message(), false, &["files-changed"]),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn reparse_inner(
    state: &AppState,
    jar: &SignedCookieJar,
    file_id: i64,
    htmx: bool,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(jar)?;

    // Fetch file record; verify ownership
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT filename, source_file, importer_id FROM imported_files WHERE id = ?",
    )
    .bind(file_id)
    .fetch_optional(&state.db)
    .await?;

    let (filename, source_file, importer_id) =
        row.ok_or_else(|| AppError::Other("Fichier introuvable".into()))?;

    if importer_id != user_id {
        return Err(AppError::Other(
            "Vous n'êtes pas autorisé à réanalyser ce fichier.".into(),
        ));
    }

    // Load the stored PDF from disk and re-run the full parse pipeline.
    let full_path = stored_file_path(state, &source_file)
        .ok_or_else(|| AppError::Other("Chemin du fichier en stockage invalide.".into()))?;
    let file_data = tokio::fs::read(&full_path).await.map_err(|e| {
        AppError::Other(format!("Impossible de lire le fichier en stockage : {}", e))
    })?;
    let bytes = bytes::Bytes::from(file_data);

    // A file with broken lines is rejected as a whole here too: nothing is added.
    let statement = safe_detect_and_parse(bytes, &filename).await.map_err(|e| {
        AppError::Other(format!("Erreur de parsing : {}", e.user_message()))
    })?;
    let total = statement.transactions.len();

    // The file's transactions still in the DB, counted by (date, operation, original
    // amount in cents): a statement can have the same line twice (two identical coffees
    // the same day).
    let existing: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT date, operation, original_amount_cents FROM transactions WHERE from_file_id = ?",
    )
    .bind(file_id)
    .fetch_all(&state.db)
    .await?;
    let missing = missing_lines(statement.transactions, existing);
    let added = missing.len() as i64;
    let skipped = (total - missing.len()) as i64;

    // Converted like an import (rates fetched before the DB transaction; a missing rate
    // adds nothing).
    let converted = convert_statement(state, &statement.currency_code, &missing)
        .await?
        .map_err(AppError::Other)?;

    // One DB transaction: either every missing transaction is added, or none.
    let mut tx = state.db.begin().await?;
    for (transaction, amounts) in missing.iter().zip(&converted.amounts) {
        // status_id 1 = pending (a file with broken lines is rejected by the parse above).
        insert_imported_transaction(&mut tx, transaction, amounts, &converted, user_id, file_id).await?;
    }
    tx.commit().await?;

    state.events.send(crate::sse::AppEvent { kind: "imports", actor_id: user_id });
    // Reparse inserts new transaction rows — notify the transactions page too.
    if added > 0 {
        state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    }
    tracing::info!(
        file_id  = %file_id,
        user_id  = %user_id,
        added    = %added,
        skipped  = %skipped,
        "Reparse complete"
    );

    if htmx {
        let msg = if added > 0 {
            format!("{} transaction(s) ajoutée(s), {} déjà présente(s).", added, skipped)
        } else {
            "Aucune transaction manquante — tout est déjà à jour.".to_string()
        };
        Ok(toast_response(&msg, true, &["files-changed"]))
    } else {
        Ok(Json(serde_json::json!({
            "ok":      true,
            "added":   added,
            "skipped": skipped,
            "total":   total,
        }))
        .into_response())
    }
}

/// The parsed lines that aren't in the DB any more. Each parsed line uses up one stored
/// transaction with the same (date, operation, original amount in cents); once none is
/// left, the line is missing (e.g. one of two identical lines was deleted).
fn missing_lines(parsed: Vec<ParsedTransaction>, stored: Vec<(String, String, i64)>) -> Vec<ParsedTransaction> {
    let mut remaining: HashMap<(String, String, i64), usize> = HashMap::new();
    for key in stored {
        *remaining.entry(key).or_insert(0) += 1;
    }
    parsed
        .into_iter()
        .filter(|t| {
            let key = (t.date.clone(), t.operation.clone(), to_cents(t.amount));
            match remaining.get_mut(&key).filter(|c| **c > 0) {
                Some(count) => { *count -= 1; false }
                None => true,
            }
        })
        .collect()
}

/// Strip non-alphanumeric characters from a filename to prevent path traversal attacks.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{check_pdf_upload, confirm_import_inner, missing_lines, reject_broken, ConfirmImportForm, PendingUpload};
    use crate::{models::ParsedTransaction, test_support::*};

    const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";

    #[test]
    fn accepts_a_pdf_and_keeps_its_name() {
        assert_eq!(check_pdf_upload("Relevé_2026-01.PDF", PDF).unwrap(), "Relevé_2026-01.PDF");
        // Only the last path component is kept; unsafe characters become `_`.
        assert_eq!(check_pdf_upload("C:\\Users\\me\\releve mars.pdf", PDF).unwrap(), "releve_mars.pdf");
        assert_eq!(check_pdf_upload("../../data/x.pdf", PDF).unwrap(), "x.pdf");
    }

    #[test]
    fn rejects_other_files() {
        assert!(check_pdf_upload("notes.txt", PDF).is_err());
        assert!(check_pdf_upload(".pdf", PDF).is_err());
        assert!(check_pdf_upload("..pdf", PDF).is_err());
        assert!(check_pdf_upload("empty.pdf", b"").is_err());
        // Right name, wrong content.
        assert!(check_pdf_upload("fake.pdf", b"<html><script>alert(1)</script></html>").is_err());
        assert!(check_pdf_upload("truncated.pdf", b"%PDF-1.7\n1 0 obj").is_err());
        assert!(check_pdf_upload(&format!("{}.pdf", "a".repeat(200)), PDF).is_err());
    }

    fn line(date: &str, operation: &str, amount: f64, parse_note: Option<&str>) -> ParsedTransaction {
        ParsedTransaction {
            date: date.into(),
            transaction_date: date.into(),
            operation: operation.into(),
            amount,
            parse_note: parse_note.map(Into::into),
        }
    }

    #[test]
    fn a_broken_line_rejects_the_file() {
        assert!(reject_broken(&[line("2026-01-05", "CB CAFE", -3.5, None)]).is_ok());
        let err = reject_broken(&[
            line("2026-01-05", "CB CAFE", -3.5, None),
            line("2026-01-06", "VIR ???", 0.0, Some("montant illisible")),
        ])
        .unwrap_err();
        assert!(err.starts_with("1 ligne(s)"), "{}", err);
        assert!(err.contains("VIR ???") && err.contains("montant illisible"), "{}", err);
    }

    /// Two identical coffees on the statement, one deleted from the app: reparse adds
    /// exactly one back; nothing when the app has them all (or more).
    #[test]
    fn reparse_finds_exactly_the_missing_lines() {
        let parsed = || vec![line("2026-01-05", "CB CAFE", -3.5, None), line("2026-01-05", "CB CAFE", -3.5, None), line("2026-01-06", "LOYER", -800.0, None)];
        let stored = |n: usize| {
            let mut v = vec![("2026-01-06".to_string(), "LOYER".to_string(), -80000)];
            v.extend(std::iter::repeat(("2026-01-05".to_string(), "CB CAFE".to_string(), -350)).take(n));
            v
        };
        let missing = missing_lines(parsed(), stored(1));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].operation, "CB CAFE");
        assert!(missing_lines(parsed(), stored(2)).is_empty());
        assert!(missing_lines(parsed(), stored(3)).is_empty());
    }

    fn upload(user_id: i64, filename: &str, content: &[u8]) -> PendingUpload {
        use sha2::{Digest, Sha256};
        PendingUpload {
            user_id,
            bank_id: 1,
            bank_name: "BNP Paribas".into(),
            currency_code: "EUR".into(),
            filename: filename.into(),
            content_hash: format!("{:x}", Sha256::digest(content)),
            bytes: bytes::Bytes::copy_from_slice(content),
            transactions: vec![line("2026-01-05", "CB CAFE", -3.5, None), line("2026-01-06", "SALAIRE", 2500.0, None)],
            created: std::time::Instant::now(),
        }
    }

    /// Confirm one analyzed upload (as the browser does with its token); `Err` = refused.
    async fn confirm(app: &TestApp, user_id: i64, upload: PendingUpload) -> Result<serde_json::Value, String> {
        let token = app.state.uploads.insert(upload);
        let form = ConfirmImportForm { token };
        match confirm_import_inner(app.state.clone(), jar_for(&app.state, user_id), form).await {
            Ok(resp) => {
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                Ok(serde_json::from_slice(&body).unwrap())
            }
            Err(e) => Err(format!("{} [{}]", e.user_message(), e)),
        }
    }

    async fn counts(app: &TestApp) -> (i64, i64) {
        sqlx::query_as("SELECT (SELECT COUNT(*) FROM imported_files), (SELECT COUNT(*) FROM transactions)")
            .fetch_one(&app.state.db)
            .await
            .unwrap()
    }

    fn stored_pdf(app: &TestApp, name: &str) -> std::path::PathBuf {
        std::path::Path::new(&app.state.config.storage_path).join("Statements/BNP Paribas").join(name)
    }

    #[tokio::test]
    async fn confirm_saves_the_file_and_its_transactions() {
        let app = test_app().await;
        let alice = add_user(&app.state.db, "Alice").await;
        let json = confirm(&app, alice, upload(alice, "releve.pdf", b"%PDF-1 janvier %%EOF")).await.unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(counts(&app).await, (1, 2));
        assert_eq!(std::fs::read(stored_pdf(&app, "releve.pdf")).unwrap(), b"%PDF-1 janvier %%EOF");
        // Stored in cents, in EUR, pending.
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT amount_cents, original_amount_cents, status_id FROM transactions ORDER BY id")
            .fetch_all(&app.state.db).await.unwrap();
        assert_eq!(rows, vec![(-350, -350, 1), (250000, 250000, 1)]);
    }

    /// Another statement with the same name is refused, and the first one's PDF is left
    /// untouched (the old upload overwrote it). Same content under another name: refused.
    #[tokio::test]
    async fn duplicates_are_refused_and_nothing_is_written() {
        let app = test_app().await;
        let alice = add_user(&app.state.db, "Alice").await;
        confirm(&app, alice, upload(alice, "releve.pdf", b"%PDF-1 janvier %%EOF")).await.unwrap();

        let err = confirm(&app, alice, upload(alice, "RELEVE.pdf", b"%PDF-1 fevrier %%EOF")).await.unwrap_err();
        assert!(err.contains("déjà été importé"), "{}", err);
        let err = confirm(&app, alice, upload(alice, "copie.pdf", b"%PDF-1 janvier %%EOF")).await.unwrap_err();
        assert!(err.contains("contenu identique"), "{}", err);

        assert_eq!(counts(&app).await, (1, 2));
        assert_eq!(std::fs::read(stored_pdf(&app, "releve.pdf")).unwrap(), b"%PDF-1 janvier %%EOF");
        assert!(!stored_pdf(&app, "copie.pdf").exists());
    }

    /// A token works once, and only for the user who uploaded the file.
    #[tokio::test]
    async fn a_token_is_single_use_and_personal() {
        let app = test_app().await;
        let alice = add_user(&app.state.db, "Alice").await;
        let bob = add_user(&app.state.db, "Bob").await;

        let token = app.state.uploads.insert(upload(alice, "releve.pdf", b"%PDF-1 janvier %%EOF"));
        let as_bob = confirm_import_inner(app.state.clone(), jar_for(&app.state, bob), ConfirmImportForm { token: token.clone() }).await;
        assert!(as_bob.is_err());
        assert_eq!(counts(&app).await, (0, 0));

        // Bob's attempt didn't consume it: Alice can still confirm, once.
        let first = confirm_import_inner(app.state.clone(), jar_for(&app.state, alice), ConfirmImportForm { token: token.clone() }).await;
        assert!(first.is_ok());
        let again = confirm_import_inner(app.state.clone(), jar_for(&app.state, alice), ConfirmImportForm { token }).await;
        assert!(again.is_err());
        assert_eq!(counts(&app).await, (1, 2));
    }
}
