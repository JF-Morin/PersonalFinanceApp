use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::{
    error::{AppError, AppResult},
    models::{Category, Currency, TransactionView},
    AppState,
};
use super::{base_context, get_user_id, is_htmx, htmx_redirect};

#[derive(Deserialize, Default)]
pub struct TransactionFilter {
    pub search: Option<String>,
    pub category_id: Option<i64>,
}

pub async fn get_transactions(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(filter): Query<TransactionFilter>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let transactions = fetch_transaction_views(&state.db, false, &filter).await?;

    // Targeted HTMX request (filter form) — return only the rows partial.
    // hx-boost navigation also sets HX-Request, so exclude it here.
    if is_htmx(&headers) && !super::is_htmx_boost(&headers) {
        let current_user_id = super::get_user_id(&jar)?;
        let mut ctx = tera::Context::new();
        ctx.insert("transactions", &transactions);
        ctx.insert("current_user_id", &current_user_id);
        let html = state.tera.render("transactions/_tx_rows.html", &ctx)?;
        return Ok(Html(html));
    }

    let mut ctx = base_context(&jar, &state.db).await?;
    let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let current_user_id = super::get_user_id(&jar)?;

    // Pending/broken transactions for the "complete" carousel (user's own only)
    let pending_filter = TransactionFilter::default();
    let all_pending = fetch_transaction_views(&state.db, true, &pending_filter).await?;
    let user_pending: Vec<&crate::models::TransactionView> = all_pending
        .iter()
        .filter(|t| t.paid_by_id == current_user_id)
        .collect();
    let pending_json = serde_json::to_string(&user_pending)
        .unwrap_or_else(|_| "[]".to_string());

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    ctx.insert("transactions", &transactions);
    ctx.insert("categories", &categories);
    ctx.insert("currencies", &currencies);
    ctx.insert("current_user_id", &current_user_id);
    ctx.insert("filter", &filter.search);
    ctx.insert("filter_category", &filter.category_id);
    ctx.insert("pending_json", &pending_json);
    ctx.insert("pending_count", &user_pending.len());
    ctx.insert("default_currency_id", &default_currency_id);

    let html = state.tera.render("transactions/list.html", &ctx)?;
    Ok(Html(html))
}


#[derive(Deserialize)]
pub struct UpdateTransactionForm {
    pub date: Option<String>,
    pub operation: Option<String>,
    pub amount: Option<f64>,
    pub currency_id: Option<i64>,
    // Kept as String so that an empty-string submission ("—" option) deserialises as None
    // rather than causing a 422 parse error before the handler even runs.
    pub category_id: Option<String>,
    pub other_share: Option<String>,
    pub notes: Option<String>,
}

pub async fn post_update_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Parse optional numeric fields — empty string means the user cleared the field (→ None).
    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let other_share: Option<f64> = form.other_share
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    // Status is determined purely from the data: both fields set → complete, either null → pending.
    let is_complete = category_id.is_some() && other_share.is_some();
    let status_id: i64 = if is_complete { 2 } else { 1 };

    sqlx::query(
        "UPDATE transactions \
         SET date             = COALESCE(?, date), \
             transaction_date = COALESCE(?, transaction_date), \
             operation        = COALESCE(?, operation), \
             amount           = COALESCE(?, amount), \
             currency_id      = COALESCE(?, currency_id), \
             category_id      = ?, \
             other_share      = ?, \
             notes            = ?, \
             status_id        = ?, \
             parse_note       = NULL \
         WHERE id = ? AND paid_by = ?",
    )
    .bind(form.date.as_deref())
    .bind(form.date.as_deref())
    .bind(form.operation.as_deref())
    .bind(form.amount)
    .bind(form.currency_id)
    .bind(category_id)
    .bind(other_share)
    .bind(&form.notes)
    .bind(status_id)
    .bind(id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    tracing::info!(id = %id, user_id = %user_id, is_complete = %is_complete, "Transaction updated");

    Ok(Json(serde_json::json!({ "ok": true, "complete": is_complete })).into_response())
}

pub async fn post_delete_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match delete_transaction_inner(&state, &jar, id).await {
        Ok(_) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response(),
    }
}

async fn delete_transaction_inner(state: &AppState, jar: &CookieJar, id: i64) -> AppResult<()> {
    let user_id = get_user_id(jar)?;

    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM transactions WHERE id = ? AND paid_by = ?",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    sqlx::query("DELETE FROM transactions WHERE id = ? AND paid_by = ?")
        .bind(id)
        .bind(user_id)
        .execute(&state.db)
        .await?;

    tracing::info!(id = %id, user_id = %user_id, "Transaction deleted");
    Ok(())
}

#[derive(Deserialize)]
pub struct ManualTransactionForm {
    pub date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_id: i64,
    pub category_id: Option<String>,
    pub other_share: Option<String>,
    pub notes: Option<String>,
}

pub async fn post_manual_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<ManualTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Manual transactions have no imported_file; we use a sentinel value.
    // We need a valid from_file_id. For manual entries, we create a pseudo import record.
    let file_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM imported_files WHERE filename = 'manual' LIMIT 1"
    )
    .fetch_optional(&state.db)
    .await?;

    let file_id = if let Some(id) = file_id {
        id
    } else {
        sqlx::query_scalar(
            "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id) VALUES ('manual', 1, '', 0, ?) RETURNING id",
        )
        .bind(user_id)
        .fetch_one(&state.db)
        .await?
    };

    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let other_share: Option<f64> = form.other_share
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let status_id: i64 = if category_id.is_some() && other_share.is_some() { 2 } else { 1 };

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    sqlx::query(
        "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, category_id, paid_by, other_share, status_id, from_file_id, notes)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&form.date)
    .bind(&form.date)
    .bind(&form.operation)
    .bind(form.amount)
    .bind(if form.currency_id > 0 { form.currency_id } else { default_currency_id })
    .bind(category_id)
    .bind(user_id)
    .bind(other_share)
    .bind(status_id)
    .bind(file_id)
    .bind(&form.notes)
    .execute(&state.db)
    .await?;

    tracing::info!(user_id = %user_id, "Manual transaction added");

    if is_htmx(&headers) {
        Ok(htmx_redirect("/transactions").into_response())
    } else {
        Ok(Json(serde_json::json!({ "ok": true })).into_response())
    }
}

async fn fetch_transaction_views(
    db: &crate::db::DbPool,
    pending_only: bool,
    filter: &TransactionFilter,
) -> AppResult<Vec<TransactionView>> {
    // 'broken' transactions surface here so the user can correct them; 'complete' are excluded.
    let status_filter = if pending_only { "AND s.name IN ('pending', 'broken')" } else { "" };
    let sql = format!(
        r#"
        SELECT t.id, t.date, t.transaction_date, t.operation, t.amount,
               cur.symbol AS currency_symbol,
               t.currency_id,
               cat.name   AS category_name,
               t.category_id,
               u.name || COALESCE(' ' || u.last_name, '') AS paid_by_name,
               t.paid_by,
               t.other_share, s.name AS status_name, t.notes, t.from_file_id,
               t.parse_note
        FROM transactions t
        JOIN currencies cur ON t.currency_id = cur.id
        LEFT JOIN categories cat ON t.category_id = cat.id
        JOIN users u ON t.paid_by = u.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE 1=1 {status_filter}
        ORDER BY t.date DESC, t.id DESC
        "#
    );

    #[allow(clippy::type_complexity)]
    let rows = sqlx::query_as::<
        _,
        (i64, String, String, String, f64, String, i64, Option<String>, Option<i64>, String, i64, Option<f64>, String, Option<String>, i64, Option<String>),
    >(&sql)
    .fetch_all(db)
    .await?;

    let mut views: Vec<TransactionView> = rows
        .into_iter()
        .map(|(id, date, td, op, amt, csym, cid, cat, cat_id, pby, pby_id, share, sname, notes, fid, parse_note)| {
            TransactionView {
                id, date, transaction_date: td, operation: op, amount: amt,
                currency_symbol: csym, currency_id: cid,
                category_name: cat, category_id: cat_id,
                paid_by_name: pby, paid_by_id: pby_id,
                other_share: share, status_name: sname, notes, from_file_id: fid,
                parse_note,
            }
        })
        .collect();

    if let Some(search) = &filter.search {
        let s = search.to_lowercase();
        views.retain(|t| t.operation.to_lowercase().contains(&s));
    }
    if let Some(cat_id) = filter.category_id {
        // category filter would need raw query — simplified: skip for now
        let _ = cat_id;
    }

    Ok(views)
}
