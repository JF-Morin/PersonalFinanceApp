use axum::{
    extract::{Multipart, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::{
    error::{AppError, AppResult},
    models::{Bank, ParsedTransaction},
    parsers::{all_parsers, detect_and_parse, detect_and_parse_positioned, lopdf_text},
    AppState,
};
use super::base_context;

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

pub async fn get_imported_files(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;
    let current_user_id = super::get_user_id(&jar)?;

    let files = sqlx::query_as::<_, ImportedFileView>(
        r#"
        SELECT f.id, f.filename, f.imported_at, f.row_count, f.source_file,
               b.name AS bank_name,
               u.name || COALESCE(' ' || u.last_name, '') AS importer_name,
               f.importer_id
        FROM imported_files f
        JOIN banks b ON f.bank_id = b.id
        JOIN users u ON f.importer_id = u.id
        WHERE f.filename != 'manual'
        ORDER BY f.imported_at DESC
        "#
    )
    .fetch_all(&state.db)
    .await?;

    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    ctx.insert("files", &files);
    ctx.insert("current_user_id", &current_user_id);
    ctx.insert("banks", &banks);
    let html = state.tera.render("imports/list.html", &ctx)?;
    Ok(Html(html))
}


#[derive(Deserialize)]
pub struct ConfirmImportForm {
    pub transactions_json: String,
    pub bank_id: i64,
    pub filename: String,
    pub source_file: String,
    pub content_hash: String,
}

pub async fn post_confirm_import(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<ConfirmImportForm>,
) -> impl IntoResponse {
    match confirm_import_inner(state, jar, headers, form).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response(),
    }
}

async fn confirm_import_inner(
    state: AppState,
    jar: CookieJar,
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

    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id, content_hash) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&form.filename)
    .bind(form.bank_id)
    .bind(&form.source_file)
    .bind(transactions.len() as i64)
    .bind(user_id)
    .bind(&form.content_hash)
    .fetch_one(&state.db)
    .await?;

    for tx in &transactions {
        let status_id: i64 = if tx.parse_note.is_some() { 3 } else { 1 };
        sqlx::query(
            "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, owner_id, status_id, from_file_id, parse_note)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&tx.date)
        .bind(&tx.transaction_date)
        .bind(&tx.operation)
        .bind(tx.amount)
        .bind(default_currency_id)
        .bind(user_id)
        .bind(status_id)
        .bind(file_id)
        .bind(&tx.parse_note)
        .execute(&state.db)
        .await?;
    }

    state.events.send(crate::sse::AppEvent { kind: "imports", actor_id: user_id });
    tracing::info!(
        file_id      = %file_id,
        filename     = %form.filename,
        bank_id      = %form.bank_id,
        transactions = %transactions.len(),
        user_id      = %user_id,
        "Import confirmed and saved"
    );

    Ok(Json(serde_json::json!({ "ok": true, "file_id": file_id, "tx_count": transactions.len() }))
        .into_response())
}

pub async fn post_analyze(
    State(state): State<AppState>,
    jar: CookieJar,
    multipart: Multipart,
) -> impl IntoResponse {
    match analyze_inner(state, jar, multipart).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response(),
    }
}

async fn analyze_inner(
    state: AppState,
    jar: CookieJar,
    mut multipart: Multipart,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    let mut file_bytes: Option<bytes::Bytes> = None;
    let mut original_name = String::from("upload.pdf");

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
    let tx_count = transactions.len();

    let transactions_json = serde_json::to_string(&transactions)
        .map_err(|e| AppError::Other(e.to_string()))?;

    tracing::info!(
        filename   = %safe_name,
        bank       = %bank_name,
        tx_count   = %tx_count,
        broken     = %broken_count,
        "Analysis complete"
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "bank_id": bank_id,
        "bank_name": bank_name,
        "tx_count": tx_count,
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
async fn safe_detect_and_parse(
    bytes: bytes::Bytes,
    filename: &str,
) -> AppResult<(i64, String, Vec<ParsedTransaction>)> {
    let bytes_clone = bytes.clone();

    // Try pdf_extract inside catch_unwind so a panic becomes an Err
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
            tracing::warn!(
                filename = %filename,
                error    = %pdf_err,
                "pdf_extract error, trying lopdf fallback"
            );
            lopdf_fallback(&bytes, filename)
        }
        Err(_panic_payload) => {
            tracing::warn!(filename = %filename, "pdf_extract panicked, trying lopdf fallback");
            lopdf_fallback(&bytes, filename)
        }
    }
}

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

#[derive(Deserialize)]
pub struct DeleteFileParams {
    #[serde(default)]
    with_transactions: Option<String>,
}

pub async fn post_delete_imported_file(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(file_id): Path<i64>,
    Query(params): Query<DeleteFileParams>,
) -> impl IntoResponse {
    match delete_file_inner(state, jar, file_id, &params).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response(),
    }
}

async fn delete_file_inner(
    state: AppState,
    jar: CookieJar,
    file_id: i64,
    params: &DeleteFileParams,
) -> AppResult<axum::response::Response> {
    let user_id = super::get_user_id(&jar)?;

    let with_transactions = params
        .with_transactions
        .as_deref()
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    // Fetch file record
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT filename, source_file FROM imported_files WHERE id = ?",
    )
    .bind(file_id)
    .fetch_optional(&state.db)
    .await?;

    let (filename, source_file) = row.ok_or_else(|| AppError::Other("Fichier introuvable".into()))?;

    let mut transactions_deleted: i64 = 0;

    if with_transactions {
        transactions_deleted = sqlx::query_scalar(
            "SELECT COUNT(*) FROM transactions WHERE from_file_id = ?",
        )
        .bind(file_id)
        .fetch_one(&state.db)
        .await?;

        sqlx::query("DELETE FROM transactions WHERE from_file_id = ?")
            .bind(file_id)
            .execute(&state.db)
            .await?;

        tracing::info!(
            file_id = %file_id,
            filename = %filename,
            transactions_deleted = %transactions_deleted,
            "Transactions deleted with file"
        );
    }

    // Delete the DB record
    sqlx::query("DELETE FROM imported_files WHERE id = ?")
        .bind(file_id)
        .execute(&state.db)
        .await?;

    // Delete the physical file from storage (best-effort — don't fail if missing)
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
    tracing::info!(
        file_id              = %file_id,
        filename             = %filename,
        with_transactions    = %with_transactions,
        transactions_deleted = %transactions_deleted,
        "Import file deleted"
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "transactions_deleted": transactions_deleted,
    }))
    .into_response())
}

// ── Reparse imported file ─────────────────────────────────────────────────────

pub async fn post_reparse_imported_file(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(file_id): Path<i64>,
) -> impl IntoResponse {
    match reparse_inner(&state, &jar, file_id).await {
        Ok(resp) => resp,
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response(),
    }
}

async fn reparse_inner(
    state: &AppState,
    jar: &CookieJar,
    file_id: i64,
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

    // Load the stored PDF
    let full_path = PathBuf::from(&state.config.storage_path).join(&source_file);
    let file_data = tokio::fs::read(&full_path).await.map_err(|e| {
        AppError::Other(format!("Impossible de lire le fichier en stockage : {}", e))
    })?;
    let bytes = bytes::Bytes::from(file_data);

    // Re-parse
    let (_, _, new_txs) = safe_detect_and_parse(bytes, &filename).await.map_err(|e| {
        AppError::Other(format!("Erreur de parsing : {}", e))
    })?;

    // Fetch existing transactions for deduplication (date + operation + amount)
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

    for tx in &new_txs {
        let already_exists = existing.iter().any(|(d, op, amt)| {
            d == &tx.date && op == &tx.operation && (amt - tx.amount).abs() < 0.005
        });

        if already_exists {
            skipped += 1;
            continue;
        }

        let status_id: i64 = if tx.parse_note.is_some() { 3 } else { 1 };
        sqlx::query(
            "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, \
             owner_id, status_id, from_file_id, parse_note) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&tx.date)
        .bind(&tx.transaction_date)
        .bind(&tx.operation)
        .bind(tx.amount)
        .bind(default_currency_id)
        .bind(user_id)
        .bind(status_id)
        .bind(file_id)
        .bind(&tx.parse_note)
        .execute(&state.db)
        .await?;

        added += 1;
    }

    state.events.send(crate::sse::AppEvent { kind: "imports", actor_id: user_id });
    tracing::info!(
        file_id  = %file_id,
        user_id  = %user_id,
        added    = %added,
        skipped  = %skipped,
        "Reparse complete"
    );

    Ok(Json(serde_json::json!({
        "ok":      true,
        "added":   added,
        "skipped": skipped,
        "total":   new_txs.len(),
    }))
    .into_response())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
