use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::{
    db::DbPool,
    error::{AppError, AppResult},
    models::{Bank, Category, Currency, ShareView, TransactionView, User},
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
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let current_user_id = super::get_user_id(&jar)?;

    let pending_filter = TransactionFilter::default();
    let all_pending = fetch_transaction_views(&state.db, true, &pending_filter).await?;
    let user_pending: Vec<TransactionView> = all_pending
        .iter()
        .filter(|t| t.owner_id == current_user_id)
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
    ctx.insert("users", &users);
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
    pub category_id: Option<String>,
    pub notes: Option<String>,
}

pub async fn post_update_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    let cur: (String, i64, Option<f64>, Option<i64>) = sqlx::query_as(
        "SELECT date, currency_id, foreign_currency_amount, foreign_currency_id \
         FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|_| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    let (cur_date, cur_currency_id, cur_fca, _) = cur;

    let effective_date        = form.date.clone().unwrap_or(cur_date);
    let effective_currency_id = form.currency_id.unwrap_or(cur_currency_id);
    let is_foreign            = effective_currency_id != default_currency_id;

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
                id=%id, foreign_amount=%foreign_amount, from=%from_code,
                to=%to_code, rate=%rate, converted=%converted,
                "Currency recalculated on transaction update"
            );
            (Some(converted), default_currency_id, Some(foreign_amount), Some(effective_currency_id))
        } else {
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
             notes                   = ?, \
             foreign_currency_amount = ?, \
             foreign_currency_id     = ?, \
             parse_note              = NULL \
         WHERE id = ? AND owner_id = ?",
    )
    .bind(&effective_date)
    .bind(&effective_date)
    .bind(form.operation.as_deref())
    .bind(final_amount)
    .bind(final_currency_id)
    .bind(category_id)
    .bind(&form.notes)
    .bind(final_fca)
    .bind(final_fci)
    .bind(id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    let is_complete = recalculate_status(&state.db, id).await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(id=%id, user_id=%user_id, is_complete=%is_complete, is_foreign=%is_foreign, "Transaction updated");

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
        "SELECT id FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    // ON DELETE CASCADE removes transaction_shares rows automatically.
    sqlx::query("DELETE FROM transactions WHERE id = ? AND owner_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(&state.db)
        .await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(id=%id, user_id=%user_id, "Transaction deleted");
    Ok(())
}

#[derive(Deserialize)]
pub struct ManualTransactionForm {
    pub date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_id: i64,
    pub category_id: Option<String>,
    pub notes: Option<String>,
}

pub async fn post_manual_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<ManualTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

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

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    let effective_currency_id = if form.currency_id > 0 { form.currency_id } else { default_currency_id };
    let is_foreign = effective_currency_id != default_currency_id;

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
                foreign_amount=%form.amount, from=%from_code, to=%to_code,
                rate=%rate, converted=%converted,
                "Currency converted for manual transaction"
            );
            (converted, default_currency_id, Some(form.amount), Some(effective_currency_id))
        } else {
            (form.amount, default_currency_id, None, None)
        };

    let new_tx_id: i64 = sqlx::query_scalar(
        "INSERT INTO transactions (date, transaction_date, operation, amount, currency_id, \
         category_id, owner_id, status_id, from_file_id, notes, \
         foreign_currency_amount, foreign_currency_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&form.date)
    .bind(&form.date)
    .bind(&form.operation)
    .bind(final_amount)
    .bind(final_currency_id)
    .bind(category_id)
    .bind(user_id)
    .bind(file_id)
    .bind(&form.notes)
    .bind(final_fca)
    .bind(final_fci)
    .fetch_one(&state.db)
    .await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(user_id=%user_id, tx_id=%new_tx_id, is_foreign=%is_foreign, "Manual transaction added");

    if is_htmx(&headers) {
        Ok(htmx_redirect("/transactions").into_response())
    } else {
        Ok(Json(serde_json::json!({ "ok": true, "id": new_tx_id })).into_response())
    }
}

// ── Share management ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ShareForm {
    pub user_id: i64,
    pub share: f64,
}

pub async fn post_transaction_share(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(tx_id): Path<i64>,
    Form(form): Form<ShareForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Only the transaction owner can manage shares.
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(tx_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    if form.share <= 0.0 || form.share > 100.0 {
        return Err(AppError::Other("La part doit être entre 0 et 100".into()));
    }

    // Verify user_id is a valid user.
    let valid_user: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
        .bind(form.user_id)
        .fetch_optional(&state.db)
        .await?;
    valid_user.ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;

    sqlx::query(
        "INSERT INTO transaction_shares (transaction_id, user_id, share)
         VALUES (?, ?, ?)
         ON CONFLICT(transaction_id, user_id) DO UPDATE SET share = excluded.share",
    )
    .bind(tx_id)
    .bind(form.user_id)
    .bind(form.share)
    .execute(&state.db)
    .await?;

    let shares = get_transaction_shares(&state.db, tx_id).await?;
    let is_complete = recalculate_status(&state.db, tx_id).await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(tx_id=%tx_id, user_id=%form.user_id, share=%form.share, "Share upserted");

    Ok(Json(serde_json::json!({ "ok": true, "shares": shares, "complete": is_complete })))
}

pub async fn delete_transaction_share(
    State(state): State<AppState>,
    jar: CookieJar,
    Path((tx_id, share_user_id)): Path<(i64, i64)>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(tx_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    sqlx::query("DELETE FROM transaction_shares WHERE transaction_id = ? AND user_id = ?")
        .bind(tx_id)
        .bind(share_user_id)
        .execute(&state.db)
        .await?;

    let shares = get_transaction_shares(&state.db, tx_id).await?;
    let is_complete = recalculate_status(&state.db, tx_id).await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(tx_id=%tx_id, share_user_id=%share_user_id, "Share removed");

    Ok(Json(serde_json::json!({ "ok": true, "shares": shares, "complete": is_complete })))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub async fn get_transaction_shares(db: &DbPool, tx_id: i64) -> AppResult<Vec<ShareView>> {
    let rows: Vec<(i64, f64, String, Option<String>)> = sqlx::query_as(
        "SELECT ts.user_id, ts.share, su.name, su.last_name \
         FROM transaction_shares ts \
         JOIN users su ON ts.user_id = su.id \
         WHERE ts.transaction_id = ? \
         ORDER BY ts.user_id",
    )
    .bind(tx_id)
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(|(user_id, share, name, last_name)| {
        let first = name.chars().next().unwrap_or(' ').to_uppercase().next().unwrap_or(' ');
        let last_char = last_name.as_deref()
            .and_then(|ln| ln.chars().next())
            .map(|c| c.to_uppercase().next().unwrap_or(' '));
        let initials = match last_char {
            Some(l) => format!("{}{}", first, l),
            None    => first.to_string(),
        };
        let user_name = match &last_name {
            Some(ln) => format!("{} {}", name, ln),
            None     => name.clone(),
        };
        ShareView { user_id, share, user_name, initials }
    }).collect())
}

/// Recalculates and writes the status for a transaction.
/// Skips broken transactions (parse_note is still set).
/// Returns true if the transaction is now complete.
async fn recalculate_status(db: &DbPool, tx_id: i64) -> AppResult<bool> {
    let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT category_id, parse_note FROM transactions WHERE id = ?",
    )
    .bind(tx_id)
    .fetch_optional(db)
    .await?;

    let (cat_id, parse_note) = match row {
        Some(r) => r,
        None    => return Ok(false),
    };

    if parse_note.is_some() {
        return Ok(false); // still broken; don't touch status
    }

    let shares_sum: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(share) FROM transaction_shares WHERE transaction_id = ?",
    )
    .bind(tx_id)
    .fetch_one(db)
    .await?;

    let is_complete = cat_id.is_some()
        && shares_sum.map(|s| (s - 100.0).abs() < 0.01).unwrap_or(false);

    let new_status_id: i64 = if is_complete { 2 } else { 1 };

    sqlx::query("UPDATE transactions SET status_id = ? WHERE id = ?")
        .bind(new_status_id)
        .bind(tx_id)
        .execute(db)
        .await?;

    Ok(is_complete)
}

// ── Fetch transaction views ───────────────────────────────────────────────────

/// sqlx FromRow intermediate — shares aggregated as a JSON string.
#[derive(sqlx::FromRow)]
struct TransactionViewRaw {
    id: i64,
    date: String,
    transaction_date: String,
    operation: String,
    amount: f64,
    currency_symbol: String,
    currency_id: i64,
    category_name: Option<String>,
    category_id: Option<i64>,
    owner_name: String,
    owner_id: i64,
    shares_json: String,
    status_name: String,
    notes: Option<String>,
    from_file_id: i64,
    parse_note: Option<String>,
    foreign_currency_amount: Option<f64>,
    foreign_currency_id: Option<i64>,
    foreign_currency_symbol: Option<String>,
}

fn raw_to_view(r: TransactionViewRaw) -> TransactionView {
    let shares: Vec<ShareView> = serde_json::from_str(&r.shares_json).unwrap_or_default();
    let shares_total: f64 = shares.iter().map(|s| s.share).sum();
    TransactionView {
        id: r.id,
        date: r.date,
        transaction_date: r.transaction_date,
        operation: r.operation,
        amount: r.amount,
        currency_symbol: r.currency_symbol,
        currency_id: r.currency_id,
        category_name: r.category_name,
        category_id: r.category_id,
        owner_name: r.owner_name,
        owner_id: r.owner_id,
        shares,
        shares_total,
        status_name: r.status_name,
        notes: r.notes,
        from_file_id: r.from_file_id,
        parse_note: r.parse_note,
        foreign_currency_amount: r.foreign_currency_amount,
        foreign_currency_id: r.foreign_currency_id,
        foreign_currency_symbol: r.foreign_currency_symbol,
    }
}

async fn fetch_transaction_views(
    db: &DbPool,
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
               u.name || COALESCE(' ' || u.last_name, '') AS owner_name,
               t.owner_id,
               COALESCE((
                   SELECT json_group_array(json_object(
                       'user_id',   ts.user_id,
                       'share',     ts.share,
                       'user_name', su.name || COALESCE(' ' || su.last_name, ''),
                       'initials',  UPPER(SUBSTR(su.name, 1, 1)) ||
                                    COALESCE(UPPER(SUBSTR(su.last_name, 1, 1)), '')
                   ))
                   FROM transaction_shares ts
                   JOIN users su ON ts.user_id = su.id
                   WHERE ts.transaction_id = t.id
               ), '[]') AS shares_json,
               s.name AS status_name, t.notes, t.from_file_id,
               t.parse_note,
               t.foreign_currency_amount,
               t.foreign_currency_id,
               fc.symbol   AS foreign_currency_symbol
        FROM transactions t
        JOIN currencies cur ON t.currency_id = cur.id
        LEFT JOIN currencies fc ON t.foreign_currency_id = fc.id
        LEFT JOIN categories cat ON t.category_id = cat.id
        JOIN users u ON t.owner_id = u.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE 1=1 {status_filter}
        ORDER BY t.date DESC, t.id DESC
        "#
    );

    let raws = sqlx::query_as::<_, TransactionViewRaw>(&sql)
        .fetch_all(db)
        .await?;

    let mut views: Vec<TransactionView> = raws.into_iter().map(raw_to_view).collect();

    if let Some(search) = &filter.search {
        let s = search.to_lowercase();
        views.retain(|t| t.operation.to_lowercase().contains(&s));
    }
    if let Some(cat_id) = filter.category_id {
        views.retain(|t| t.category_id == Some(cat_id));
    }

    Ok(views)
}
