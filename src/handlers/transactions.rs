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
    models::{Bank, Category, Currency, TransactionView},
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
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let current_user_id = super::get_user_id(&jar)?;

    // Pending/broken transactions for the carousel — current user's own first,
    // fall back to all pending if the user has none (e.g. imported under a different user id).
    let pending_filter = TransactionFilter::default();
    let all_pending = fetch_transaction_views(&state.db, true, &pending_filter).await?;
    let user_pending: Vec<crate::models::TransactionView> = all_pending
        .iter()
        .filter(|t| t.paid_by_id == current_user_id)
        .cloned()
        .collect();
    let carousel_items = if !user_pending.is_empty() { &user_pending } else { &all_pending };
    let pending_json = serde_json::to_string(carousel_items)
        .unwrap_or_else(|_| "[]".to_string());

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    ctx.insert("transactions", &transactions);
    ctx.insert("categories", &categories);
    ctx.insert("currencies", &currencies);
    ctx.insert("banks", &banks);
    ctx.insert("current_user_id", &current_user_id);
    ctx.insert("filter", &filter.search);
    ctx.insert("filter_category", &filter.category_id);
    ctx.insert("pending_json", &pending_json);
    ctx.insert("pending_count", &carousel_items.len());
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

    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let other_share: Option<f64> = form.other_share
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let is_complete = category_id.is_some() && other_share.is_some();
    let status_id: i64 = if is_complete { 2 } else { 1 };

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    // Fetch current state so we can fall back when fields are omitted.
    let cur: (String, i64, Option<f64>, Option<i64>) = sqlx::query_as(
        "SELECT date, currency_id, foreign_currency_amount, foreign_currency_id \
         FROM transactions WHERE id = ? AND paid_by = ?",
    )
    .bind(id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| crate::error::AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    let (cur_date, cur_currency_id, cur_fca, _) = cur;

    let effective_date       = form.date.clone().unwrap_or(cur_date);
    let effective_currency_id = form.currency_id.unwrap_or(cur_currency_id);
    let is_foreign            = effective_currency_id != default_currency_id;

    // If the effective currency is foreign (or remains foreign), convert the amount.
    // The amount field in the form represents the foreign amount when currency is foreign.
    let (final_amount, final_currency_id, final_fca, final_fci): (Option<f64>, i64, Option<f64>, Option<i64>) =
        if is_foreign {
            let foreign_amount = form.amount.or(cur_fca).unwrap_or(0.0);
            let from_code: String = sqlx::query_scalar("SELECT code FROM currencies WHERE id = ?")
                .bind(effective_currency_id)
                .fetch_one(&state.db)
                .await
                .unwrap_or_default();
            let to_code: String = sqlx::query_scalar("SELECT code FROM currencies WHERE id = ?")
                .bind(default_currency_id)
                .fetch_one(&state.db)
                .await
                .unwrap_or_default();
            let rate = crate::currency::get_exchange_rate(&from_code, &to_code, &effective_date)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(from=%from_code, to=%to_code, date=%effective_date, err=%e, "Exchange rate fetch failed; using 1.0");
                    1.0
                });
            let converted = foreign_amount * rate;
            tracing::info!(
                id = %id, foreign_amount = %foreign_amount, from = %from_code,
                to = %to_code, rate = %rate, converted = %converted,
                "Currency recalculated on transaction update"
            );
            (Some(converted), default_currency_id, Some(foreign_amount), Some(effective_currency_id))
        } else {
            // Default currency: clear any existing foreign fields, use amount as-is.
            (form.amount, default_currency_id, None, None)
        };

    sqlx::query(
        "UPDATE transactions \
         SET date                    = ?, \
             transaction_date        = ?, \
             operation               = COALESCE(?, operation), \
             amount                  = COALESCE(?, amount), \
             currency_id             = ?, \
             category_id             = ?, \
             other_share             = ?, \
             notes                   = ?, \
             status_id               = ?, \
             foreign_currency_amount = ?, \
             foreign_currency_id     = ?, \
             parse_note              = NULL \
         WHERE id = ? AND paid_by = ?",
    )
    .bind(&effective_date)
    .bind(&effective_date)
    .bind(form.operation.as_deref())
    .bind(final_amount)
    .bind(final_currency_id)
    .bind(category_id)
    .bind(other_share)
    .bind(&form.notes)
    .bind(status_id)
    .bind(final_fca)
    .bind(final_fci)
    .bind(id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    tracing::info!(id = %id, user_id = %user_id, is_complete = %is_complete, is_foreign = %is_foreign, "Transaction updated");

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

    let effective_currency_id = if form.currency_id > 0 { form.currency_id } else { default_currency_id };
    let is_foreign = effective_currency_id != default_currency_id;

    // If the selected currency differs from the default, fetch the live exchange rate and
    // store the original amount in foreign_currency_amount; amount holds the converted value.
    let (final_amount, final_currency_id, final_fca, final_fci): (f64, i64, Option<f64>, Option<i64>) =
        if is_foreign {
            let from_code: String = sqlx::query_scalar("SELECT code FROM currencies WHERE id = ?")
                .bind(effective_currency_id)
                .fetch_one(&state.db)
                .await
                .unwrap_or_default();
            let to_code: String = sqlx::query_scalar("SELECT code FROM currencies WHERE id = ?")
                .bind(default_currency_id)
                .fetch_one(&state.db)
                .await
                .unwrap_or_default();
            let rate = crate::currency::get_exchange_rate(&from_code, &to_code, &form.date)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(from=%from_code, to=%to_code, date=%form.date, err=%e, "Exchange rate fetch failed; using 1.0");
                    1.0
                });
            let converted = form.amount * rate;
            tracing::info!(
                foreign_amount = %form.amount, from = %from_code, to = %to_code,
                rate = %rate, converted = %converted,
                "Currency converted for manual transaction"
            );
            (converted, default_currency_id, Some(form.amount), Some(effective_currency_id))
        } else {
            (form.amount, default_currency_id, None, None)
        };

    sqlx::query(
        "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, \
         category_id, paid_by, other_share, status_id, from_file_id, notes, \
         foreign_currency_amount, foreign_currency_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&form.date)
    .bind(&form.date)
    .bind(&form.operation)
    .bind(final_amount)
    .bind(final_currency_id)
    .bind(category_id)
    .bind(user_id)
    .bind(other_share)
    .bind(status_id)
    .bind(file_id)
    .bind(&form.notes)
    .bind(final_fca)
    .bind(final_fci)
    .execute(&state.db)
    .await?;

    tracing::info!(user_id = %user_id, is_foreign = %is_foreign, "Manual transaction added");

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
    let status_filter = if pending_only { "AND s.name IN ('pending', 'broken')" } else { "" };
    let sql = format!(
        r#"
        SELECT t.id, t.date, t.transaction_date, t.operation, t.amount,
               cur.symbol  AS currency_symbol,
               t.currency_id,
               cat.name    AS category_name,
               t.category_id,
               u.name || COALESCE(' ' || u.last_name, '') AS paid_by_name,
               t.paid_by   AS paid_by_id,
               t.other_share, s.name AS status_name, t.notes, t.from_file_id,
               t.parse_note,
               t.foreign_currency_amount,
               t.foreign_currency_id,
               fc.symbol   AS foreign_currency_symbol
        FROM transactions t
        JOIN currencies cur ON t.currency_id = cur.id
        LEFT JOIN currencies fc ON t.foreign_currency_id = fc.id
        LEFT JOIN categories cat ON t.category_id = cat.id
        JOIN users u ON t.paid_by = u.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE 1=1 {status_filter}
        ORDER BY t.date DESC, t.id DESC
        "#
    );

    let mut views: Vec<TransactionView> = sqlx::query_as::<_, TransactionView>(&sql)
        .fetch_all(db)
        .await?;

    if let Some(search) = &filter.search {
        let s = search.to_lowercase();
        views.retain(|t| t.operation.to_lowercase().contains(&s));
    }
    if let Some(cat_id) = filter.category_id {
        let _ = cat_id;
    }

    Ok(views)
}
