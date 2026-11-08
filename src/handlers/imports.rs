/// Import handlers — PDF upload, analysis, confirmation, deletion, and reparse.
///
/// The import flow is two-step:
///   1. POST /upload/analyze   — receive the PDF, extract text, run bank parsers, return
///                               a JSON preview of parsed transactions to the browser.
///   2. POST /upload/confirm   — browser sends back the JSON plus form fields; we write the
///                               `imported_files` row and all `transactions` rows to the DB.
///
/// Two-step prevents "phantom" imports when the user cancels the preview modal.
/// The file is NOT written to storage until the user confirms.
///
/// Duplicate detection uses a SHA-256 hash of the PDF bytes; the hash is stored in
/// `imported_files.content_hash` and checked at both analyze and confirm time.
///
/// For PDFs that use Type3 fonts (which crash pdf_extract), we fall back to lopdf-based
/// positioned text extraction where column X coordinates distinguish debits from credits.
use axum::{
    extract::{Multipart, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json},
    Form,
};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::{
    error::{AppError, AppResult},
    models::{Bank, ParsedTransaction},
    parsers::{all_parsers, detect_and_parse, detect_and_parse_positioned, lopdf_text},
    AppState,
};
use super::{base_context, is_htmx, is_htmx_boost, toast_response};

/// Enriched file row joined with bank name and importer name for the imports table.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ImportedFileView {
    id: i64,
    filename: String,
    imported_at: String,
    row_count: i64,
    source_file: String,
    bank_name: String,
    importer_name: String,
    importer_id: i64,
}

/// `?sort=<column>&dir=asc|desc` on the imported files list.
#[derive(Deserialize, Default)]
pub struct FilesSortParams {
    sort: Option<String>,
    dir: Option<String>,
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
    ("filename",     "Fichier",      "16rem", "f.filename COLLATE NOCASE"),
    ("bank",         "Banque",       "10rem", "b.name COLLATE NOCASE"),
    ("date",         "Importé le",   "12rem", "f.imported_at"),
    ("transactions", "Transactions", "6rem",  "row_count"),
    ("importer",     "Importé par",  "10rem", "importer_name COLLATE NOCASE"),
];

/// Render the files table (`pages/imports/_table.html`) into `ctx`'s variables.
/// Unknown sort keys fall back to the default: newest import first.
async fn insert_files_table(
    ctx: &mut tera::Context,
    db: &crate::db::DbPool,
    params: &FilesSortParams,
    current_user_id: i64,
) -> AppResult<()> {
    let (sort, order_by) = FILE_COLUMNS
        .iter()
        .find(|c| Some(c.0) == params.sort.as_deref())
        .map(|c| (c.0, c.3))
        .unwrap_or(("date", "f.imported_at"));
    let dir = match params.dir.as_deref() {
        Some("asc") => "asc",
        Some("desc") => "desc",
        _ if sort == "date" => "desc",
        _ => "asc",
    };

    // `order_by` and `dir` come from the whitelists above, never from user input.
    let sql = format!(
        r#"
        SELECT f.id, f.filename, f.imported_at,
               -- Current count: `f.row_count` is frozen at import time and goes stale when
               -- single transactions are deleted or re-added by a reparse.
               (SELECT COUNT(*) FROM transactions t WHERE t.from_file_id = f.id) AS row_count,
               f.source_file,
               b.name AS bank_name,
               u.name || COALESCE(' ' || u.last_name, '') AS importer_name,
               f.importer_id
        FROM imported_files f
        JOIN banks b ON f.bank_id = b.id
        JOIN users u ON f.importer_id = u.id
        WHERE f.filename != 'manual'
        ORDER BY {order_by} {dir}, f.id DESC
        "#
    );
    let files = sqlx::query_as::<_, ImportedFileView>(&sql).fetch_all(db).await?;

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

    ctx.insert("files", &files);
    ctx.insert("columns", &columns);
    ctx.insert("sort", sort);
    ctx.insert("dir", dir);
    ctx.insert("current_user_id", &current_user_id);
    Ok(())
}

/// GET /imported-files — render the imported files list page.
/// HTMX requests (sorting, SSE refresh) get only the table fragment.
/// The synthetic "manual" row is excluded from the table (it's an implementation detail).
pub async fn get_imported_files(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(params): Query<FilesSortParams>,
) -> AppResult<Html<String>> {
    let current_user_id = super::get_user_id(&jar)?;

    if is_htmx(&headers) && !is_htmx_boost(&headers) {
        let mut ctx = tera::Context::new();
        insert_files_table(&mut ctx, &state.db, &params, current_user_id).await?;
        let html = state.tera.render("pages/imports/_table.html", &ctx)?;
        return Ok(Html(html));
    }

    let mut ctx = base_context(&jar, &state.db, "/imported-files").await?;
    insert_files_table(&mut ctx, &state.db, &params, current_user_id).await?;
    let html = state.tera.render("pages/imports/imports.html", &ctx)?;
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

/// Form submitted by the import confirmation modal.
#[derive(Deserialize)]
pub struct ConfirmImportForm {
    /// The parsed transactions JSON that was returned from the analyze step and echoed back.
    pub transactions_json: String,
    pub bank_id: i64,
    pub filename: String,
    /// Relative path under STORAGE_PATH where the file was already written.
    pub source_file: String,
    /// SHA-256 hex digest of the original PDF bytes (checked again server-side).
    pub content_hash: String,
}

/// POST /upload/confirm — persist the analyzed transactions to the DB.
pub async fn post_confirm_import(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<ConfirmImportForm>,
) -> impl IntoResponse {
    match confirm_import_inner(state, jar, headers, form).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn confirm_import_inner(
    state: AppState,
    jar: SignedCookieJar,
    _headers: HeaderMap,
    form: ConfirmImportForm,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    // Server-side bank check (mirrors analyze_inner; prevents direct API bypass).
    let user_bank: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT u.bank_id, b.name FROM users u LEFT JOIN banks b ON u.bank_id = b.id WHERE u.id = ?",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some((Some(user_bank_id), ref user_bank_name)) = user_bank {
        if user_bank_id != form.bank_id {
            let linked = user_bank_name.as_deref().unwrap_or("votre banque");
            return Err(AppError::Other(format!(
                "Importation refusée : ce fichier ne correspond pas à votre banque ({}).",
                linked
            )));
        }
    }

    // Hard block: same content hash means same PDF, regardless of filename.
    let already_exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM imported_files WHERE content_hash = ? LIMIT 1",
    )
    .bind(&form.content_hash)
    .fetch_optional(&state.db)
    .await?;

    if already_exists.is_some() {
        return Err(AppError::Other(
            "Ce fichier a déjà été importé (contenu identique). Supprimez l'import existant avant de réimporter.".to_string(),
        ));
    }

    let transactions: Vec<ParsedTransaction> =
        serde_json::from_str(&form.transactions_json)
            .map_err(|e| AppError::Other(format!("Invalid transaction data: {}", e)))?;

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    // One DB transaction: the file row and all its transactions, or nothing.
    let mut tx = state.db.begin().await?;
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id, content_hash) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&form.filename)
    .bind(form.bank_id)
    .bind(&form.source_file)
    .bind(transactions.len() as i64)
    .bind(user_id)
    .bind(&form.content_hash)
    .fetch_one(&mut *tx)
    .await?;

    for transaction in &transactions {
        // status_id: 1 = pending, 3 = broken (parse_note present).
        let status_id: i64 = if transaction.parse_note.is_some() { 3 } else { 1 };
        sqlx::query(
            "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, owner_id, status_id, from_file_id, parse_note)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&transaction.date)
        .bind(&transaction.transaction_date)
        .bind(&transaction.operation)
        .bind(transaction.amount)
        .bind(default_currency_id)
        .bind(user_id)
        .bind(status_id)
        .bind(file_id)
        .bind(&transaction.parse_note)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // Notify imports listeners (imports page) and transactions listeners (transactions page,
    // home page) — this operation writes rows to both tables.
    state.events.send(crate::sse::AppEvent { kind: "imports",      actor_id: user_id });
    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(
        file_id      = %file_id,
        filename     = %form.filename,
        bank_id      = %form.bank_id,
        transactions = %transactions.len(),
        user_id      = %user_id,
        "Import confirmed and saved"
    );

    Ok(Json(serde_json::json!({ "ok": true, "file_id": file_id, "transaction_count": transactions.len() }))
        .into_response())
}

/// POST /upload/analyze — receive a PDF, parse it, return a JSON preview (no DB writes).
pub async fn post_analyze(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    multipart: Multipart,
) -> impl IntoResponse {
    match analyze_inner(state, jar, multipart).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.user_message() })).into_response(),
    }
}

async fn analyze_inner(
    state: AppState,
    jar: SignedCookieJar,
    mut multipart: Multipart,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    let mut file_bytes: Option<bytes::Bytes> = None;
    let mut original_name = String::from("upload.pdf");

    // Read the multipart stream until we find the "file" field.
    while let Some(field) = multipart.next_field().await.map_err(|e| AppError::Other(e.to_string()))? {
        if field.name() == Some("file") {
            original_name = field.file_name().unwrap_or("upload.pdf").to_string();
            file_bytes = Some(field.bytes().await.map_err(|e| AppError::Other(e.to_string()))?);
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::Other("No file provided".into()))?;

    tracing::info!(filename = %original_name, bytes = %bytes.len(), "Analyze request received");

    let (bank_id, bank_name, transactions) =
        safe_detect_and_parse(bytes.clone(), &original_name).await?;

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
            return Ok(Json(serde_json::json!({
                "ok": false,
                "error": format!(
                    "Ce fichier provient de {} mais votre compte est associé à {}. Vous ne pouvez importer que vos propres relevés de banque.",
                    bank_name, linked
                )
            })).into_response());
        }
    }

    // Compute hash and check for duplicates BEFORE writing to storage.
    let content_hash = format!("{:x}", Sha256::digest(&bytes));

    let existing_import: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, imported_at FROM imported_files WHERE content_hash = ? LIMIT 1",
    )
    .bind(&content_hash)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    if let Some((existing_file_id, imported_at)) = existing_import {
        tracing::info!(
            filename         = %original_name,
            imported_at      = %imported_at,
            existing_file_id = %existing_file_id,
            "Duplicate content hash — offering reparse"
        );
        return Ok(Json(serde_json::json!({
            "ok": false,
            "duplicate": true,
            "existing_file_id": existing_file_id,
            "imported_at": imported_at,
        })).into_response());
    }

    // Save file to storage only after successful parse, bank check, and duplicate check.
    let storage_root = PathBuf::from(&state.config.storage_path);
    let statements_dir = storage_root.join("Statements").join(&bank_name);
    tokio::fs::create_dir_all(&statements_dir).await?;

    let safe_name = sanitize_filename(&original_name);
    let dest_path = statements_dir.join(&safe_name);
    tokio::fs::write(&dest_path, &bytes).await?;

    let relative_path = format!("Statements/{}/{}", bank_name, safe_name);

    let broken_count = transactions.iter().filter(|t| t.parse_note.is_some()).count();
    let transaction_count = transactions.len();

    let transactions_json = serde_json::to_string(&transactions)
        .map_err(|e| AppError::Other(e.to_string()))?;

    tracing::info!(
        filename   = %safe_name,
        bank       = %bank_name,
        transaction_count   = %transaction_count,
        broken     = %broken_count,
        "Analysis complete"
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "bank_id": bank_id,
        "bank_name": bank_name,
        "transaction_count": transaction_count,
        "broken_count": broken_count,
        "transactions_json": transactions_json,
        "filename": safe_name,
        "source_file": relative_path,
        "content_hash": content_hash,
    })).into_response())
}

/// Extract text from a PDF and run bank detection + parsing.
///
/// Tries pdf_extract first. If that panics (e.g. Type3 fonts, lopdf 0.34 issue),
/// falls back to lopdf-based positioned text extraction and the positioned parser path.
/// `catch_unwind` is used to convert a panic into a Result so the server keeps running.
async fn safe_detect_and_parse(
    bytes: bytes::Bytes,
    filename: &str,
) -> AppResult<(i64, String, Vec<ParsedTransaction>)> {
    let bytes_clone = bytes.clone();

    // Run pdf_extract in a blocking thread (it's CPU-bound) and catch any panics.
    let text_result = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text_from_mem(&bytes_clone)
        }))
    })
    .await
    .map_err(|e| AppError::Other(format!("spawn_blocking join error: {}", e)))?;

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
                match lopdf_fallback(&bytes, filename) {
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
            lopdf_fallback(&bytes, filename)
        }
        Err(_panic_payload) => {
            // pdf_extract panicked — try lopdf.
            tracing::warn!(filename = %filename, "pdf_extract panicked, trying lopdf fallback");
            lopdf_fallback(&bytes, filename)
        }
    }
}

/// lopdf-based extraction path: returns positioned text items (char, x, y per page)
/// so parsers can determine debit/credit by X column position.
fn lopdf_fallback(
    bytes: &bytes::Bytes,
    filename: &str,
) -> AppResult<(i64, String, Vec<ParsedTransaction>)> {
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
    let physical_path = PathBuf::from(&state.config.storage_path).join(&source_file);
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

    if htmx {
        let msg = format!("Fichier supprimé — {} transaction(s) supprimée(s).", transactions_deleted);
        Ok(toast_response(&msg, true, &["files-changed"]))
    } else {
        Ok(Json(serde_json::json!({
            "ok": true,
            "transactions_deleted": transactions_deleted,
        }))
        .into_response())
    }
}

// ── Reparse imported file ─────────────────────────────────────────────────────

/// POST /imported-files/:id/reparse — re-run the parser and add any transactions that
/// weren't present in the original import (e.g., because the parser was improved later).
/// Existing transactions are compared by (date, operation, amount) to avoid duplicates.
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
    let full_path = PathBuf::from(&state.config.storage_path).join(&source_file);
    let file_data = tokio::fs::read(&full_path).await.map_err(|e| {
        AppError::Other(format!("Impossible de lire le fichier en stockage : {}", e))
    })?;
    let bytes = bytes::Bytes::from(file_data);

    let (_, _, new_transactions) = safe_detect_and_parse(bytes, &filename).await.map_err(|e| {
        AppError::Other(format!("Erreur de parsing : {}", e))
    })?;

    // Fetch existing transactions for deduplication (date + operation + amount).
    let existing: Vec<(String, String, f64)> = sqlx::query_as(
        "SELECT date, operation, amount FROM transactions WHERE from_file_id = ?",
    )
    .bind(file_id)
    .fetch_all(&state.db)
    .await?;

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    let mut added: i64 = 0;
    let mut skipped: i64 = 0;

    // One DB transaction: either every missing transaction is added, or none.
    let mut tx = state.db.begin().await?;
    for transaction in &new_transactions {
        // Skip if a transaction with the same date, operation, and amount already exists.
        // 0.005 tolerance on amount to handle minor floating-point differences.
        let already_exists = existing.iter().any(|(d, op, amt)| {
            d == &transaction.date && op == &transaction.operation && (amt - transaction.amount).abs() < 0.005
        });

        if already_exists {
            skipped += 1;
            continue;
        }

        let status_id: i64 = if transaction.parse_note.is_some() { 3 } else { 1 };
        sqlx::query(
            "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, \
             owner_id, status_id, from_file_id, parse_note) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&transaction.date)
        .bind(&transaction.transaction_date)
        .bind(&transaction.operation)
        .bind(transaction.amount)
        .bind(default_currency_id)
        .bind(user_id)
        .bind(status_id)
        .bind(file_id)
        .bind(&transaction.parse_note)
        .execute(&mut *tx)
        .await?;

        added += 1;
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
            "total":   new_transactions.len(),
        }))
        .into_response())
    }
}

/// Strip non-alphanumeric characters from a filename to prevent path traversal attacks.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
