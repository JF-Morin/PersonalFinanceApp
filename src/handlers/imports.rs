use axum::{
    extract::{Multipart, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use std::path::PathBuf;

use crate::{
    error::{AppError, AppResult},
    models::{Bank, ParsedTransaction},
    parsers::detect_and_parse,
    AppState,
};
use super::{base_context, is_htmx, htmx_redirect};

pub async fn get_imported_files(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    let files = sqlx::query!(
        r#"
        SELECT f.id, f.filename, f.imported_at, f.row_count, f.source_file,
               b.name AS bank_name,
               u.name || COALESCE(' ' || u.last_name, '') AS importer_name
        FROM imported_files f
        JOIN banks b ON f.bank_id = b.id
        JOIN users u ON f.importer_id = u.id
        WHERE f.filename != 'manual'
        ORDER BY f.imported_at DESC
        "#
    )
    .fetch_all(&state.db)
    .await?;

    ctx.insert("files", &files);
    let html = state.tera.render("imports/list.html", &ctx)?;
    Ok(Html(html))
}

pub async fn get_upload(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    ctx.insert("banks", &banks);
    ctx.insert("parse_error", &Option::<String>::None);
    ctx.insert("parsed_transactions", &Option::<Vec<ParsedTransaction>>::None);

    let html = state.tera.render("imports/upload.html", &ctx)?;
    Ok(Html(html))
}

pub async fn post_upload(
    State(state): State<AppState>,
    jar: CookieJar,
    _headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    let mut file_bytes: Option<bytes::Bytes> = None;
    let mut original_name = String::from("upload.pdf");

    while let Some(field) = multipart.next_field().await.map_err(|e| AppError::Other(e.to_string()))? {
        if field.name() == Some("file") {
            original_name = field
                .file_name()
                .unwrap_or("upload.pdf")
                .to_string();
            file_bytes = Some(field.bytes().await.map_err(|e| AppError::Other(e.to_string()))?);
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::Other("No file provided".into()))?;

    tracing::info!(filename = %original_name, bytes = %bytes.len(), user_id = %user_id, "PDF upload received");

    // Extract text from PDF in a blocking thread
    let bytes_clone = bytes.clone();
    let text = tokio::task::spawn_blocking(move || {
        pdf_extract::extract_text_from_mem(&bytes_clone)
            .map_err(|e| anyhow::anyhow!("PDF extraction failed: {}", e))
    })
    .await
    .map_err(|e| AppError::Other(e.to_string()))?
    .map_err(|e| AppError::Other(e.to_string()))?;

    // Detect bank and parse
    let (bank_id, bank_name, transactions) = match detect_and_parse(&text) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(filename = %original_name, error = %e, "No parser matched uploaded file");
            let mut ctx = tera::Context::new();
            ctx.insert("parse_error", &Some(e.to_string()));
            ctx.insert("parsed_transactions", &Option::<Vec<ParsedTransaction>>::None);
            let html = state.tera.render("imports/_preview.html", &ctx)?;
            return Ok(Html(html).into_response());
        }
    };

    // Save file to storage
    let storage_root = PathBuf::from(&state.config.storage_path);
    let statements_dir = storage_root.join("Statements").join(&bank_name);
    tokio::fs::create_dir_all(&statements_dir).await?;

    let safe_name = sanitize_filename(&original_name);
    let dest_path = statements_dir.join(&safe_name);
    tokio::fs::write(&dest_path, &bytes).await?;
    tracing::info!(path = %dest_path.display(), "PDF saved to storage");

    let relative_path = format!("Statements/{}/{}", bank_name, safe_name);
    let transactions_json = serde_json::to_string(&transactions)
        .map_err(|e| AppError::Other(e.to_string()))?;

    let mut ctx = tera::Context::new();
    ctx.insert("parse_error", &Option::<String>::None);
    ctx.insert("parsed_transactions", &Some(&transactions));
    ctx.insert("transactions_json", &transactions_json);
    ctx.insert("bank_id", &bank_id);
    ctx.insert("bank_name", &bank_name);
    ctx.insert("filename", &safe_name);
    ctx.insert("source_file", &relative_path);
    ctx.insert("user_id", &user_id);

    let html = state.tera.render("imports/_preview.html", &ctx)?;
    Ok(Html(html).into_response())
}

#[derive(Deserialize)]
pub struct ConfirmImportForm {
    pub transactions_json: String,
    pub bank_id: i64,
    pub filename: String,
    pub source_file: String,
}

pub async fn post_confirm_import(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<ConfirmImportForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    let transactions: Vec<ParsedTransaction> =
        serde_json::from_str(&form.transactions_json)
            .map_err(|e| AppError::Other(format!("Invalid transaction data: {}", e)))?;

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    let file_id = sqlx::query_scalar!(
        "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id) VALUES (?, ?, ?, ?, ?) RETURNING id",
        form.filename,
        form.bank_id,
        form.source_file,
        transactions.len() as i64,
        user_id,
    )
    .fetch_one(&state.db)
    .await?;

    for tx in &transactions {
        sqlx::query(
            "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, paid_by, status_id, from_file_id)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(&tx.date)
        .bind(&tx.transaction_date)
        .bind(&tx.operation)
        .bind(tx.amount)
        .bind(default_currency_id)
        .bind(user_id)
        .bind(file_id)
        .execute(&state.db)
        .await?;
    }

    tracing::info!(
        file_id      = %file_id,
        filename     = %form.filename,
        bank_id      = %form.bank_id,
        transactions = %transactions.len(),
        user_id      = %user_id,
        "Import confirmed and saved"
    );

    if is_htmx(&headers) {
        Ok(htmx_redirect("/incomplete-transactions").into_response())
    } else {
        Ok(Redirect::to("/incomplete-transactions").into_response())
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
