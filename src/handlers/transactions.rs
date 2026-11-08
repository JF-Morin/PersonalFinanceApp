/// Transaction CRUD, share management, and carousel/edit support.
///
/// A transaction moves through three statuses:
///   pending  — imported from PDF, waiting for category + shares to be set by the user.
///   broken   — the PDF parser matched the row but couldn't extract all fields;
///              the user must correct operation/amount in the carousel before it can complete.
///   complete — category is set AND transaction_shares rows sum exactly to 100%.
///
/// The main table (GET /transactions) always returns ALL transactions; the carousel
/// (GET /transactions/pending-json) returns only pending/broken ones for the logged-in user.
///
/// Share management uses a separate `transaction_shares` table so multiple users can
/// each own a percentage of a single transaction (e.g., rent split 50/50).
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
use super::{base_context, get_user_id, is_htmx};

/// Optional search/category filter submitted via query string (?search=...&category_id=...).
#[derive(Deserialize, Default)]
pub struct TransactionFilter {
    pub search: Option<String>,
    pub category_id: Option<i64>,
}

/// GET /transactions — full page or partial tbody rows.
///
/// When called by HTMX (not boost), returns only the `_table_rows.html` fragment so the
/// table body can be swapped in-place without reloading the sidebar, filters, etc.
pub async fn get_transactions(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(filter): Query<TransactionFilter>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let transactions = fetch_transaction_views(&state.db, false, &filter).await?;

    // Partial refresh: HTMX GET for `hx-swap="innerHTML"` on the tbody.
    if is_htmx(&headers) && !super::is_htmx_boost(&headers) {
        let current_user_id = super::get_user_id(&jar)?;
        let mut ctx = tera::Context::new();
        ctx.insert("transactions", &transactions);
        ctx.insert("current_user_id", &current_user_id);
        let html = state.tera.render("pages/transactions/_table_rows.html", &ctx)?;
        return Ok(Html(html));
    }

    // Full page render — includes filters, modals, carousel data, etc.
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

    // The carousel is seeded with the current user's pending transactions as JSON
    // so it can render immediately without an extra round-trip.
    let pending_filter = TransactionFilter::default();
    let all_pending = fetch_transaction_views(&state.db, true, &pending_filter).await?;
    let user_pending: Vec<TransactionView> = all_pending
        .into_iter()
        .filter(|t| t.owner_id == current_user_id)
        .collect();
    let pending_json = serde_json::to_string(&user_pending)
        .unwrap_or_else(|_| "[]".to_string());
    let pending_count = user_pending.len();

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
    ctx.insert("pending_count", &pending_count);
    ctx.insert("default_currency_id", &default_currency_id);

    let html = state.tera.render("pages/transactions/transactions.html", &ctx)?;
    Ok(Html(html))
}

/// Form submitted by the edit modal / carousel "save" button.
#[derive(Deserialize)]
pub struct UpdateTransactionForm {
    pub date: Option<String>,
    pub operation: Option<String>,
    pub amount: Option<f64>,
    pub currency_id: Option<i64>,
    pub category_id: Option<String>,
    pub notes: Option<String>,
}

/// POST /transactions/:id/update — apply edits from the carousel or the edit modal.
///
/// Returns the updated row HTML when called from HTMX (the row swaps in-place),
/// or JSON when called programmatically (fetch API from the carousel JS).
pub async fn post_update_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Empty string from an unset <select> must become None, not Some("").
    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let default_currency_id: i64 = crate::db::get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    // Fetch the current row to fill in any fields the form didn't touch.
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
    // A "foreign" transaction is one whose currency differs from the app default.
    let is_foreign            = effective_currency_id != default_currency_id;

    // If the currency is foreign, fetch the exchange rate and convert to the default currency.
    // The original foreign amount is stored separately for display purposes.
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

    // `COALESCE(?, column)` means: use the form value if provided, otherwise keep the existing value.
    // `parse_note = NULL` clears any broken-parser note now that the user has corrected the row.
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

    if is_htmx(&headers) {
        // Return the refreshed row HTML so HTMX can swap it in-place.
        Ok(render_transaction_row(&state, id, user_id).await?.into_response())
    } else {
        // The carousel uses fetch() (not HTMX), so it expects JSON.
        Ok(Json(serde_json::json!({ "ok": true, "complete": is_complete })).into_response())
    }
}

/// POST /transactions/:id/delete — delete a transaction owned by the current user.
/// Returns empty HTML on success (HTMX removes the row via `hx-swap="outerHTML"`)
/// or an error body with status 500 so the UI can display it.
pub async fn post_delete_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match delete_transaction_inner(&state, &jar, id).await {
        Ok(_) => {
            if is_htmx(&headers) {
                "".into_response()
            } else {
                Json(serde_json::json!({ "ok": true })).into_response()
            }
        }
        Err(e) => {
            if is_htmx(&headers) {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            } else {
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })).into_response()
            }
        }
    }
}

async fn delete_transaction_inner(state: &AppState, jar: &CookieJar, id: i64) -> AppResult<()> {
    let user_id = get_user_id(jar)?;

    // Verify ownership before deleting — users can only delete their own transactions.
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

/// Form for manually-added transactions (from the "+ Ajouter" modal on the transactions page).
#[derive(Deserialize)]
pub struct ManualTransactionForm {
    pub date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_id: i64,
    pub category_id: Option<String>,
    pub notes: Option<String>,
    /// JSON-encoded list of `{ user_id, share }` objects built by the modal's JS.
    pub shares_json: Option<String>,
}

/// POST /transactions/manual — create a transaction directly (not from a PDF).
///
/// Manual transactions are linked to a synthetic `imported_files` row with
/// filename = 'manual' so the rest of the codebase can treat them uniformly.
/// That sentinel row is created on demand the first time a manual transaction is added.
///
/// The form uses `hx-swap="none"` so the response body is ignored; the client calls
/// `refreshTransactionTable()` in `htmx:afterRequest` to reload the full table.
pub async fn post_manual_transaction(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<ManualTransactionForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Look up or create the synthetic "manual" import record.
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

    // For foreign-currency amounts, fetch the exchange rate and store both
    // the converted amount (in the default currency) and the original amount.
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

    // Insert with status_id = 1 (pending).  recalculate_status below may promote it to 2 (complete).
    let new_transaction_id: i64 = sqlx::query_scalar(
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

    // Insert shares from the JSON payload built by `htmx:configRequest` in the modal.
    if let Some(ref sj) = form.shares_json {
        if let Ok(shares) = serde_json::from_str::<Vec<serde_json::Value>>(sj) {
            for share in &shares {
                let uid = share["user_id"].as_i64().unwrap_or(0);
                let pct = share["share"].as_f64().unwrap_or(0.0);
                if uid > 0 && pct > 0.0 && pct <= 100.0 {
                    sqlx::query(
                        "INSERT INTO transaction_shares (transaction_id, user_id, share) \
                         VALUES (?, ?, ?) \
                         ON CONFLICT(transaction_id, user_id) DO UPDATE SET share = excluded.share",
                    )
                    .bind(new_transaction_id).bind(uid).bind(pct)
                    .execute(&state.db).await?;
                }
            }
            // Now that shares are in place, check if the transaction is immediately complete.
            recalculate_status(&state.db, new_transaction_id).await?;
        }
    }

    // Broadcast to all other connected users so their tables refresh.
    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(user_id=%user_id, transaction_id=%new_transaction_id, is_foreign=%is_foreign, "Manual transaction added");

    // The form uses hx-swap="none" — the client ignores the body and calls
    // refreshTransactionTable() after htmx:afterRequest.  No need to render the row here,
    // which also avoids the SQLite WAL snapshot race that plagued the old afterbegin approach.
    if is_htmx(&headers) {
        Ok(Html(String::new()).into_response())
    } else {
        Ok(Json(serde_json::json!({ "ok": true, "id": new_transaction_id })).into_response())
    }
}

// ── Share management ──────────────────────────────────────────────────────────

/// Form for adding or updating a single user's share in a transaction.
#[derive(Deserialize)]
pub struct ShareForm {
    pub user_id: i64,
    /// Percentage, 0–100.
    pub share: f64,
}

/// POST /transactions/:id/shares — upsert a share for one user.
/// Used by the edit modal's live-save share inputs (each blur triggers a fetch POST).
pub async fn post_transaction_share(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(transaction_id): Path<i64>,
    Form(form): Form<ShareForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    // Only the transaction owner can manage shares.
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(transaction_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    if form.share <= 0.0 || form.share > 100.0 {
        return Err(AppError::Other("La part doit être entre 0 et 100".into()));
    }

    // Verify the target user exists.
    let valid_user: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
        .bind(form.user_id)
        .fetch_optional(&state.db)
        .await?;
    valid_user.ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;

    // ON CONFLICT upsert: set the new share if the row already exists.
    sqlx::query(
        "INSERT INTO transaction_shares (transaction_id, user_id, share)
         VALUES (?, ?, ?)
         ON CONFLICT(transaction_id, user_id) DO UPDATE SET share = excluded.share",
    )
    .bind(transaction_id)
    .bind(form.user_id)
    .bind(form.share)
    .execute(&state.db)
    .await?;

    let shares = get_transaction_shares(&state.db, transaction_id).await?;
    let is_complete = recalculate_status(&state.db, transaction_id).await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(transaction_id=%transaction_id, user_id=%form.user_id, share=%form.share, "Share upserted");

    // Return the updated shares list as JSON so the modal JS can re-render the pills.
    Ok(Json(serde_json::json!({ "ok": true, "shares": shares, "complete": is_complete })))
}

/// DELETE /transactions/:id/shares/:user_id — remove a user's share.
pub async fn delete_transaction_share(
    State(state): State<AppState>,
    jar: CookieJar,
    Path((transaction_id, share_user_id)): Path<(i64, i64)>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)?;

    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(transaction_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    exists.ok_or_else(|| AppError::Other("Transaction introuvable ou non autorisée".into()))?;

    sqlx::query("DELETE FROM transaction_shares WHERE transaction_id = ? AND user_id = ?")
        .bind(transaction_id)
        .bind(share_user_id)
        .execute(&state.db)
        .await?;

    let shares = get_transaction_shares(&state.db, transaction_id).await?;
    let is_complete = recalculate_status(&state.db, transaction_id).await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(transaction_id=%transaction_id, share_user_id=%share_user_id, "Share removed");

    Ok(Json(serde_json::json!({ "ok": true, "shares": shares, "complete": is_complete })))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Fetch all share rows for a transaction with display-ready user names and initials.
pub async fn get_transaction_shares(db: &DbPool, transaction_id: i64) -> AppResult<Vec<ShareView>> {
    let rows: Vec<(i64, f64, String, Option<String>)> = sqlx::query_as(
        "SELECT ts.user_id, ts.share, su.name, su.last_name \
         FROM transaction_shares ts \
         JOIN users su ON ts.user_id = su.id \
         WHERE ts.transaction_id = ? \
         ORDER BY ts.user_id",
    )
    .bind(transaction_id)
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

/// Recalculate and persist the status of a transaction.
///
/// A transaction is `complete` when:
///   - It has a category set, AND
///   - The sum of all transaction_shares rows equals exactly 100%
///   - It is NOT broken (no parse_note)
///
/// This is called after every edit, share upsert/delete, or manual transaction creation
/// so the status always reflects the current state.
async fn recalculate_status(db: &DbPool, transaction_id: i64) -> AppResult<bool> {
    let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT category_id, parse_note FROM transactions WHERE id = ?",
    )
    .bind(transaction_id)
    .fetch_optional(db)
    .await?;

    let (cat_id, parse_note) = match row {
        Some(r) => r,
        None    => return Ok(false),
    };

    // A broken transaction cannot become complete — the user must fix the parse error first.
    if parse_note.is_some() {
        return Ok(false);
    }

    let shares_sum: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(share) FROM transaction_shares WHERE transaction_id = ?",
    )
    .bind(transaction_id)
    .fetch_one(db)
    .await?;

    // 0.01 tolerance to handle floating-point rounding (e.g. 33.33 + 33.33 + 33.34 = 100.00).
    let is_complete = cat_id.is_some()
        && shares_sum.map(|s| (s - 100.0).abs() < 0.01).unwrap_or(false);

    // status_id 1 = pending, 2 = complete (see transaction_status seed data in migrations).
    let new_status_id: i64 = if is_complete { 2 } else { 1 };

    sqlx::query("UPDATE transactions SET status_id = ? WHERE id = ?")
        .bind(new_status_id)
        .bind(transaction_id)
        .execute(db)
        .await?;

    Ok(is_complete)
}

// ── Fetch transaction views ───────────────────────────────────────────────────

/// Raw sqlx FromRow struct for the big transaction JOIN query.
///
/// We can't deserialize directly into `TransactionView` because:
///   - SQLite's `json_group_array` returns a JSON string, not a typed array.
///   - Tera doesn't know how to render raw foreign-key integers.
/// So we fetch into this raw struct, then convert to `TransactionView` in `raw_to_view`.
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
    /// JSON string from `json_group_array(...)` — parsed into `Vec<ShareView>` in raw_to_view.
    shares_json: String,
    status_name: String,
    notes: Option<String>,
    from_file_id: i64,
    /// 1 when the linked imported_files row has filename = 'manual', else 0.
    is_manual: i64,
    parse_note: Option<String>,
    foreign_currency_amount: Option<f64>,
    foreign_currency_id: Option<i64>,
    foreign_currency_symbol: Option<String>,
}

/// Convert a raw DB row into the display-ready `TransactionView`.
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
        // SQLite stores booleans as integers; convert 1/0 to Rust bool.
        is_manual: r.is_manual != 0,
        parse_note: r.parse_note,
        foreign_currency_amount: r.foreign_currency_amount,
        foreign_currency_id: r.foreign_currency_id,
        foreign_currency_symbol: r.foreign_currency_symbol,
    }
}

/// Fetch all transactions (or only pending/broken ones) with all display data joined in.
///
/// The shares subquery uses `json_group_array` to aggregate each transaction's shares
/// into a single JSON column so we don't multiply rows in the result set.
/// Search and category filters are applied in Rust after fetching all rows — this is
/// fine because the dataset is small and avoids dynamic SQL construction.
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
               CASE WHEN imf.filename = 'manual' THEN 1 ELSE 0 END AS is_manual,
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
        LEFT JOIN imported_files imf ON t.from_file_id = imf.id
        WHERE 1=1 {status_filter}
        ORDER BY t.date DESC, t.id DESC
        "#
    );

    let raws = sqlx::query_as::<_, TransactionViewRaw>(&sql)
        .fetch_all(db)
        .await?;

    let mut views: Vec<TransactionView> = raws.into_iter().map(raw_to_view).collect();

    // Client-side filtering (search and category) applied after the fetch.
    if let Some(search) = &filter.search {
        let s = search.to_lowercase();
        views.retain(|t| t.operation.to_lowercase().contains(&s));
    }
    if let Some(cat_id) = filter.category_id {
        views.retain(|t| t.category_id == Some(cat_id));
    }

    Ok(views)
}

/// GET /transactions/pending-json — returns the current user's pending transactions as JSON.
/// Used by the carousel to reload its data after a save.
pub async fn get_pending_json(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    let current_user_id = get_user_id(&jar)?;
    let filter = TransactionFilter::default();
    let all_pending = fetch_transaction_views(&state.db, true, &filter).await?;
    let user_pending: Vec<TransactionView> = all_pending
        .into_iter()
        .filter(|t| t.owner_id == current_user_id)
        .collect();
    Ok(Json(user_pending))
}

/// GET /transactions/pending-badge — returns the pending count badge HTML fragment.
/// Refreshed by SSE so the sidebar badge stays current without a full reload.
pub async fn get_pending_badge(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM transactions t \
         JOIN transaction_status s ON t.status_id = s.id \
         WHERE s.name IN ('pending', 'broken') AND t.owner_id = ?",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    let mut ctx = tera::Context::new();
    ctx.insert("pending_count", &pending_count);
    let html = state.tera.render("pages/transactions/_pending_badge.html", &ctx)?;
    Ok(Html(html))
}

/// Fetch a single transaction by ID with all display data joined in (same query as the list).
async fn fetch_single_transaction_view(db: &DbPool, transaction_id: i64) -> AppResult<TransactionView> {
    let sql = r#"SELECT t.id, t.date, t.transaction_date, t.operation, t.amount,
           cur.symbol AS currency_symbol, t.currency_id,
           cat.name AS category_name, t.category_id,
           u.name || COALESCE(' ' || u.last_name, '') AS owner_name, t.owner_id,
           COALESCE((
               SELECT json_group_array(json_object(
                   'user_id', ts.user_id, 'share', ts.share,
                   'user_name', su.name || COALESCE(' ' || su.last_name, ''),
                   'initials', UPPER(SUBSTR(su.name, 1, 1)) || COALESCE(UPPER(SUBSTR(su.last_name, 1, 1)), '')
               ))
               FROM transaction_shares ts JOIN users su ON ts.user_id = su.id
               WHERE ts.transaction_id = t.id
           ), '[]') AS shares_json,
           s.name AS status_name, t.notes, t.from_file_id,
           CASE WHEN imf.filename = 'manual' THEN 1 ELSE 0 END AS is_manual,
           t.parse_note,
           t.foreign_currency_amount, t.foreign_currency_id,
           fc.symbol AS foreign_currency_symbol
    FROM transactions t
    JOIN currencies cur ON t.currency_id = cur.id
    LEFT JOIN currencies fc ON t.foreign_currency_id = fc.id
    LEFT JOIN categories cat ON t.category_id = cat.id
    JOIN users u ON t.owner_id = u.id
    JOIN transaction_status s ON t.status_id = s.id
    LEFT JOIN imported_files imf ON t.from_file_id = imf.id
    WHERE t.id = ?"#;
    let raw = sqlx::query_as::<_, TransactionViewRaw>(sql).bind(transaction_id).fetch_one(db).await?;
    Ok(raw_to_view(raw))
}

/// Render a single transaction row using the shared `_table_rows.html` partial.
/// Used by the update handler to return the refreshed row for HTMX in-place swap.
async fn render_transaction_row(state: &AppState, transaction_id: i64, user_id: i64) -> AppResult<Html<String>> {
    let transaction = fetch_single_transaction_view(&state.db, transaction_id).await?;
    let mut ctx = tera::Context::new();
    // The template iterates `transactions`, so we wrap the single item in a vec.
    ctx.insert("transactions", &vec![transaction]);
    ctx.insert("current_user_id", &user_id);
    let html = state.tera.render("pages/transactions/_table_rows.html", &ctx)?;
    Ok(Html(html))
}
