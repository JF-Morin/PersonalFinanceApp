use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::{
    error::AppResult,
    models::{Category, Currency, TransactionView, User},
    AppState,
};
use super::{base_context, is_htmx, htmx_redirect};

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

    // HTMX filter request — return only the table rows partial
    if is_htmx(&headers) {
        let mut ctx = tera::Context::new();
        ctx.insert("transactions", &transactions);
        let html = state.tera.render("transactions/_tx_rows.html", &ctx)?;
        return Ok(Html(html));
    }

    let mut ctx = base_context(&jar, &state.db).await?;
    let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    ctx.insert("transactions", &transactions);
    ctx.insert("categories", &categories);
    ctx.insert("filter", &filter.search);
    ctx.insert("filter_category", &filter.category_id);

    let html = state.tera.render("transactions/list.html", &ctx)?;
    Ok(Html(html))
}

pub async fn get_incomplete(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    let filter = TransactionFilter::default();
    let all_incomplete = fetch_transaction_views(&state.db, true, &filter).await?;
    let (broken_transactions, pending_transactions): (Vec<TransactionView>, Vec<TransactionView>) =
        all_incomplete.into_iter().partition(|t| t.status_name == "broken");

    let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    ctx.insert("broken_transactions", &broken_transactions);
    ctx.insert("transactions", &pending_transactions);
    ctx.insert("categories", &categories);
    ctx.insert("users", &users);
    ctx.insert("currencies", &currencies);

    let html = state.tera.render("transactions/incomplete.html", &ctx)?;
    Ok(Html(html))
}

#[derive(Deserialize)]
pub struct UpdateTransactionForm {
    pub operation: Option<String>,
    pub amount: Option<f64>,
    pub category_id: Option<i64>,
    pub other_share: Option<f64>,
    pub notes: Option<String>,
    pub mark_complete: Option<String>,
}

pub async fn post_update_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let _ = super::get_user_id(&jar)?;

    let marking_complete = form.mark_complete.as_deref() == Some("1")
        && form.category_id.is_some()
        && form.other_share.is_some();

    let status_id: i64 = if marking_complete { 2 } else { 1 };

    // operation/amount are only submitted from the broken-transaction fix form.
    if form.operation.is_some() || form.amount.is_some() {
        // COALESCE keeps the stored value if the field was somehow omitted; parse_note is cleared.
        sqlx::query(
            "UPDATE transactions SET operation = COALESCE(?, operation), amount = COALESCE(?, amount), category_id = ?, other_share = ?, notes = ?, status_id = ?, parse_note = NULL WHERE id = ?",
        )
        .bind(form.operation.as_deref())
        .bind(form.amount)
        .bind(form.category_id)
        .bind(form.other_share)
        .bind(&form.notes)
        .bind(status_id)
        .bind(id)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query(
            "UPDATE transactions SET category_id = ?, other_share = ?, notes = ?, status_id = ? WHERE id = ?",
        )
        .bind(form.category_id)
        .bind(form.other_share)
        .bind(&form.notes)
        .bind(status_id)
        .bind(id)
        .execute(&state.db)
        .await?;
    }

    if is_htmx(&headers) {
        if marking_complete {
            // Empty response with outerHTML swap removes the card from the DOM
            return Ok("".into_response());
        }
        return Ok(htmx_redirect("/incomplete-transactions").into_response());
    }
    Ok(Redirect::to("/incomplete-transactions").into_response())
}

#[derive(Deserialize)]
pub struct ManualTransactionForm {
    pub date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_id: i64,
    pub category_id: Option<i64>,
    pub other_share: Option<f64>,
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
    let file_id = sqlx::query_scalar!(
        "SELECT id FROM imported_files WHERE filename = 'manual' LIMIT 1"
    )
    .fetch_optional(&state.db)
    .await?;

    let file_id = if let Some(id) = file_id {
        id
    } else {
        sqlx::query_scalar!(
            "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id) VALUES ('manual', 1, '', 0, ?) RETURNING id",
            user_id
        )
        .fetch_one(&state.db)
        .await?
    };

    let status_id: i64 = if form.category_id.is_some() && form.other_share.is_some() { 2 } else { 1 };

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
    .bind(form.category_id)
    .bind(user_id)
    .bind(form.other_share)
    .bind(status_id)
    .bind(file_id)
    .bind(&form.notes)
    .execute(&state.db)
    .await?;

    if is_htmx(&headers) {
        Ok(htmx_redirect("/").into_response())
    } else {
        Ok(Redirect::to("/").into_response())
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
               cat.name   AS category_name,
               u.name || COALESCE(' ' || u.last_name, '') AS paid_by_name,
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

    let rows = sqlx::query_as::<
        _,
        (i64, String, String, String, f64, String, Option<String>, String, Option<f64>, String, Option<String>, i64, Option<String>),
    >(&sql)
    .fetch_all(db)
    .await?;

    let mut views: Vec<TransactionView> = rows
        .into_iter()
        .map(|(id, date, td, op, amt, csym, cat, pby, share, sname, notes, fid, parse_note)| {
            TransactionView {
                id, date, transaction_date: td, operation: op, amount: amt,
                currency_symbol: csym, category_name: cat, paid_by_name: pby,
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
