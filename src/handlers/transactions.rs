/// Transaction CRUD, shares, the carousel, and CSV export.
///
/// A transaction moves through three statuses:
///   pending  — imported from PDF, waiting for category + shares to be set by the user.
///   broken   — the PDF parser matched the row but couldn't extract all fields;
///              the user must correct operation/amount in the carousel before it can complete.
///   complete — category is set AND transaction_shares rows sum exactly to 100%.
///
/// The page is server-driven: filters and sorting are query parameters (`TransactionFilter`),
/// the table is re-rendered as a fragment, and every modal (edit, delete, manual entry,
/// carousel card) is rendered here and swapped into `#modal-root`. Results come back as
/// HX-Trigger events (toast, `close-modal`, `transactions-changed`).
///
/// Share management uses a separate `transaction_shares` table so multiple users can
/// each own a percentage of a single transaction (e.g., rent split 50/50). Forms submit
/// them as one `shares_json` field (built by `modal/_shares_field.html`).
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    response::{Html, IntoResponse, Response},
    Form,
};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;
use std::collections::HashMap;

use crate::{
    db::DbPool,
    error::{AppError, AppResult},
    models::{Category, Currency, ShareView, TransactionView, User},
    AppState,
};
use super::{
    base_context, events_header, get_user_id, htmx_inline_error, is_htmx, is_htmx_boost,
    toast_header, toast_response,
};

// ── Filters and sorting ───────────────────────────────────────────────────────

/// Query string of the transactions page (`?search=&category_id=&year=&sort=&dir=&limit=`).
/// Every field is a string because an empty `<input>` / `<select>` submits "".
/// `year` absent means "the current year" (see `default_to_current_year`); `year=` (empty,
/// the "Toutes" button) means all years.
#[derive(Deserialize, Default)]
pub struct TransactionFilter {
    pub search: Option<String>,
    pub category_id: Option<String>,
    pub year: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    /// How many rows the table shows (scroll loading asks for PAGE_SIZE more each time).
    pub limit: Option<usize>,
}

/// Rows per scroll step on the transactions page.
const PAGE_SIZE: usize = 100;

impl TransactionFilter {
    fn search(&self) -> Option<&str> {
        self.search.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    fn category_id(&self) -> Option<i64> {
        self.category_id.as_deref().and_then(|s| s.parse().ok())
    }

    fn year(&self) -> Option<&str> {
        self.year
            .as_deref()
            .filter(|y| y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()))
    }

    /// (column, direction); unknown values fall back to the default, newest first.
    fn sort(&self) -> (&'static str, &'static str) {
        let col = match self.sort.as_deref() {
            Some("category") => "category",
            Some("owner") => "owner",
            Some("amount") => "amount",
            _ => "date",
        };
        let dir = match self.dir.as_deref() {
            Some("asc") => "asc",
            Some("desc") => "desc",
            _ if col == "date" => "desc",
            _ => "asc",
        };
        (col, dir)
    }

    /// The filters (not the sort) as a query string, e.g. `search=loyer&year=2026`.
    fn filter_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = self.search() {
            parts.push(format!("search={}", url_encode(s)));
        }
        if let Some(c) = self.category_id() {
            parts.push(format!("category_id={}", c));
        }
        // Always explicit: no `year` would mean "current year", so "all years" is `year=`.
        parts.push(format!("year={}", self.year().unwrap_or("")));
        parts.join("&")
    }

    /// Rows to show: at least one page, more once the user has scrolled.
    fn limit(&self) -> usize {
        self.limit.unwrap_or(PAGE_SIZE).clamp(PAGE_SIZE, 100_000)
    }

    /// No `year` in the URL → the current year (the page's default view).
    async fn default_to_current_year(&mut self, db: &DbPool) -> AppResult<()> {
        if self.year.is_none() {
            let year: String = sqlx::query_scalar("SELECT strftime('%Y', 'now', 'localtime')")
                .fetch_one(db)
                .await?;
            self.year = Some(year);
        }
        Ok(())
    }

    /// Filters + sort, for URLs that must reproduce the current table exactly.
    fn full_query(&self) -> String {
        let (sort, dir) = self.sort();
        format!("{}&sort={}&dir={}", self.filter_query(), sort, dir)
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Apply the search / category / year filters and the sort to an already-fetched list.
/// Rows arrive newest first and `sort_by` is stable, so ties keep that order.
fn filter_and_sort(mut views: Vec<TransactionView>, filter: &TransactionFilter) -> Vec<TransactionView> {
    if let Some(s) = filter.search() {
        let s = s.to_lowercase();
        views.retain(|t| t.operation.to_lowercase().contains(&s));
    }
    if let Some(c) = filter.category_id() {
        views.retain(|t| t.category_id == Some(c));
    }
    if let Some(y) = filter.year() {
        views.retain(|t| t.date.starts_with(y));
    }
    let (col, dir) = filter.sort();
    views.sort_by(|a, b| {
        let ord = match col {
            "category" => a.category_name.as_deref().unwrap_or("").to_lowercase()
                .cmp(&b.category_name.as_deref().unwrap_or("").to_lowercase()),
            "owner" => a.owner_name.to_lowercase().cmp(&b.owner_name.to_lowercase()),
            "amount" => a.amount.total_cmp(&b.amount),
            _ => a.date.cmp(&b.date),
        };
        if dir == "desc" { ord.reverse() } else { ord }
    });
    views
}

/// One table header, as `_table.html` needs it.
#[derive(Serialize)]
struct TableColumn {
    label: &'static str,
    style: &'static str,
    /// Sort key, or "" for a column that can't be sorted.
    key: &'static str,
    /// "asc" / "desc" when the table is sorted by this column, "" otherwise.
    dir: &'static str,
    /// Direction a click on this header switches to.
    next_dir: &'static str,
}

/// (label, style, sort key) of the data columns, between the icon and action columns.
const TABLE_COLUMNS: [(&str, &str, &str); 7] = [
    ("Date",         "width:1%;white-space:nowrap", "date"),
    ("Opération",    "min-width:20rem",             ""),
    ("Catégorie",    "min-width:6rem",              "category"),
    ("Propriétaire", "width:1%;white-space:nowrap", "owner"),
    ("Participants", "min-width:6rem",              ""),
    ("Note",         "min-width:16rem",             ""),
    ("Montant",      "min-width:13rem",             "amount"),
];

/// Context for `pages/transactions/_table.html` (also used inside the full page).
/// `transactions` is the full filtered list; only the first `limit` rows are rendered.
fn insert_table(ctx: &mut tera::Context, transactions: &[TransactionView], filter: &TransactionFilter, user_id: i64) {
    let limit = filter.limit();
    let total = transactions.len();
    let shown = &transactions[..total.min(limit)];
    let (sort, dir) = filter.sort();
    let columns: Vec<TableColumn> = TABLE_COLUMNS
        .iter()
        .map(|&(label, style, key)| {
            let active = if !key.is_empty() && key == sort { dir } else { "" };
            TableColumn { label, style, key, dir: active, next_dir: if active == "asc" { "desc" } else { "asc" } }
        })
        .collect();
    ctx.insert("columns", &columns);
    ctx.insert("transactions", shown);
    ctx.insert("total", &total);
    ctx.insert("has_more", &(total > limit));
    // Self-refresh keeps as many rows as are loaded (no jump back to the top);
    // the scroll sentinel asks for one more page.
    let full = filter.full_query();
    ctx.insert("refresh_query", &if limit > PAGE_SIZE { format!("{}&limit={}", full, limit) } else { full.clone() });
    ctx.insert("next_query", &format!("{}&limit={}", full, limit + PAGE_SIZE));
    ctx.insert("current_user_id", &user_id);
    ctx.insert("sort", sort);
    ctx.insert("dir", dir);
    ctx.insert("filter_query", &filter.filter_query());
    ctx.insert("full_query", &filter.full_query());
}

// ── Page and table ────────────────────────────────────────────────────────────

/// GET /transactions — full page, or (HTMX, not boost) only the table fragment
/// `_table.html`, which also updates the sort inputs / CSV link out-of-band.
pub async fn get_transactions(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(mut filter): Query<TransactionFilter>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    filter.default_to_current_year(&state.db).await?;
    let transactions = filter_and_sort(fetch_transaction_views(&state.db, false).await?, &filter);

    if is_htmx(&headers) && !is_htmx_boost(&headers) {
        let mut ctx = tera::Context::new();
        insert_table(&mut ctx, &transactions, &filter, user_id);
        ctx.insert("oob", &true);
        let html = state.tera.render("pages/transactions/_table.html", &ctx)?;
        return Ok(Html(html));
    }

    let mut ctx = base_context(&jar, &state.db, "/transactions").await?;
    insert_table(&mut ctx, &transactions, &filter, user_id);
    ctx.insert("oob", &false);

    let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let years: Vec<String> = sqlx::query_scalar(
        "SELECT substr(date, 1, 4) AS y FROM transactions \
         UNION SELECT strftime('%Y', 'now', 'localtime') ORDER BY y DESC",
    )
    .fetch_all(&state.db)
    .await?;

    ctx.insert("categories", &categories);
    ctx.insert("years", &years);
    ctx.insert("search", &filter.search().unwrap_or(""));
    ctx.insert("category_id", &filter.category_id());
    ctx.insert("year", &filter.year().unwrap_or(""));
    ctx.insert("pending_count", &pending_count(&state.db, user_id).await?);

    let html = state.tera.render("pages/transactions/transactions.html", &ctx)?;
    Ok(Html(html))
}

/// GET /transactions/export.csv — the filtered, sorted table as CSV (same query as the page,
/// but every matching row — no scroll limit).
pub async fn get_export_csv(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(mut filter): Query<TransactionFilter>,
) -> AppResult<Response> {
    get_user_id(&jar)?;
    filter.default_to_current_year(&state.db).await?;
    let transactions = filter_and_sort(fetch_transaction_views(&state.db, false).await?, &filter);

    let cell = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    // The BOM makes Excel open the file as UTF-8.
    let mut csv = String::from("\u{feff}Date,Opération,Catégorie,Propriétaire,Participants,Note,Montant\n");
    for t in &transactions {
        let participants = if t.shares.is_empty() {
            "(N/A)".to_string()
        } else {
            t.shares.iter().map(|s| format!("{} {:.2}%", s.initials, s.share)).collect::<Vec<_>>().join(" ")
        };
        let mut amount = format!(
            "{}{:.2} {}",
            if t.amount < 0.0 { "−" } else { "+" },
            t.amount.abs(),
            t.currency_symbol
        );
        if let (Some(fa), Some(fs)) = (t.foreign_currency_amount, &t.foreign_currency_symbol) {
            amount.push_str(&format!(" [{:.2} {}]", fa.abs(), fs));
        }
        let row = [
            cell(&t.date),
            cell(&t.operation),
            cell(t.category_name.as_deref().unwrap_or("(N/A)")),
            cell(&t.owner_name),
            cell(&participants),
            cell(t.notes.as_deref().filter(|n| !n.is_empty()).unwrap_or("—")),
            cell(&amount),
        ];
        csv.push_str(&row.join(","));
        csv.push('\n');
    }

    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"transactions.csv\""),
        ],
        csv,
    )
        .into_response())
}

// ── Modals (rendered into #modal-root) ────────────────────────────────────────

/// Select options and participants every transaction form needs.
async fn insert_form_options(ctx: &mut tera::Context, db: &DbPool, user_id: i64) -> AppResult<()> {
    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(db)
        .await?;
    let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories ORDER BY name")
        .fetch_all(db)
        .await?;
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(db)
        .await?;
    ctx.insert("currencies", &currencies);
    ctx.insert("categories", &categories);
    ctx.insert("users", &users);
    ctx.insert("current_user_id", &user_id);
    Ok(())
}

/// `{user_id: share}` — initial values for `modal/_shares_field.html`.
fn shares_init(t: &TransactionView) -> HashMap<String, f64> {
    t.shares.iter().map(|s| (s.user_id.to_string(), s.share)).collect()
}

/// Fetch a transaction the current user owns (only owners may edit or delete).
async fn owned_transaction(db: &DbPool, user_id: i64, id: i64) -> AppResult<TransactionView> {
    match fetch_single_transaction_view(db, id).await {
        Ok(t) if t.owner_id == user_id => Ok(t),
        _ => Err(AppError::Other("Transaction introuvable ou non autorisée".into())),
    }
}

async fn default_currency_id(db: &DbPool) -> i64 {
    crate::db::get_setting(db, "default_currency_id")
        .await
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// GET /transactions/new — the manual-entry modal.
pub async fn get_manual_modal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    let mut ctx = tera::Context::new();
    insert_form_options(&mut ctx, &state.db, user_id).await?;
    let today: String = sqlx::query_scalar("SELECT date('now', 'localtime')")
        .fetch_one(&state.db)
        .await?;
    ctx.insert("today", &today);
    ctx.insert("default_currency_id", &default_currency_id(&state.db).await);
    ctx.insert("shares_init", &HashMap::<String, f64>::new());
    Ok(Html(state.tera.render("modal/transaction_manual.html", &ctx)?))
}

/// GET /transactions/:id/edit — the edit modal, pre-filled.
pub async fn get_edit_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    let t = owned_transaction(&state.db, user_id, id).await?;
    let mut ctx = tera::Context::new();
    insert_form_options(&mut ctx, &state.db, user_id).await?;
    // Foreign-currency transactions are edited in their original currency.
    let (amount, currency_id) = match (t.foreign_currency_amount, t.foreign_currency_id) {
        (Some(fa), Some(fc)) => (fa, fc),
        _ => (t.amount, t.currency_id),
    };
    ctx.insert("edit_amount", &amount);
    ctx.insert("edit_currency_id", &currency_id);
    ctx.insert("shares_init", &shares_init(&t));
    ctx.insert("t", &t);
    Ok(Html(state.tera.render("modal/transaction_edit.html", &ctx)?))
}

/// GET /transactions/:id/delete — the delete confirmation modal.
pub async fn get_delete_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    let t = owned_transaction(&state.db, user_id, id).await?;
    let mut ctx = tera::Context::new();
    ctx.insert("t", &t);
    Ok(Html(state.tera.render("modal/transaction_delete.html", &ctx)?))
}

// ── Updating ──────────────────────────────────────────────────────────────────

/// Fields submitted by the edit modal and the carousel. Absent fields keep their
/// current value (the carousel has no date/currency, and operation/amount only for
/// broken lines); `shares_json` absent means "don't touch the shares".
#[derive(Deserialize)]
pub struct UpdateTransactionForm {
    pub date: Option<String>,
    pub operation: Option<String>,
    /// A string so an emptied field is "keep" rather than a 422 from the extractor.
    pub amount: Option<String>,
    pub currency_id: Option<i64>,
    pub category_id: Option<String>,
    pub notes: Option<String>,
    pub shares_json: Option<String>,
    /// Carousel only: which button was clicked — "prev", "next", "stay" or "close".
    pub nav: Option<String>,
}

/// Parse `shares_json` (`[{"user_id": 1, "share": 50}, …]`) and check the 100 % limit.
fn parse_shares(json: &str) -> Result<Vec<(i64, f64)>, String> {
    #[derive(Deserialize)]
    struct Share {
        user_id: i64,
        share: f64,
    }
    let list: Vec<Share> = serde_json::from_str(json).map_err(|_| "Parts invalides.".to_string())?;
    let shares: Vec<(i64, f64)> = list.into_iter().filter(|s| s.share > 0.0).map(|s| (s.user_id, s.share)).collect();
    if shares.iter().map(|s| s.1).sum::<f64>() > 100.001 {
        return Err("La somme des parts ne peut pas dépasser 100 %.".into());
    }
    Ok(shares)
}

/// Replace all share rows of a transaction (on the caller's DB transaction).
async fn replace_shares(conn: &mut SqliteConnection, transaction_id: i64, shares: &[(i64, f64)]) -> AppResult<()> {
    sqlx::query("DELETE FROM transaction_shares WHERE transaction_id = ?")
        .bind(transaction_id)
        .execute(&mut *conn)
        .await?;
    for &(user_id, share) in shares {
        sqlx::query("INSERT INTO transaction_shares (transaction_id, user_id, share) VALUES (?, ?, ?)")
            .bind(transaction_id)
            .bind(user_id)
            .bind(share)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Apply an edit from the modal or the carousel. `Ok(Err(msg))` is a user-facing
/// validation error (shown inside the modal); `Ok(Ok(complete))` reports the new status.
async fn update_transaction(
    state: &AppState,
    user_id: i64,
    id: i64,
    form: &UpdateTransactionForm,
) -> AppResult<Result<bool, String>> {
    let shares = match form.shares_json.as_deref() {
        Some(json) => match parse_shares(json) {
            Ok(shares) => Some(shares),
            Err(msg) => return Ok(Err(msg)),
        },
        None => None,
    };
    let amount: Option<f64> = match form.amount.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => match a.replace(',', ".").parse() {
            Ok(v) => Some(v),
            Err(_) => return Ok(Err("Montant invalide.".into())),
        },
        None => None,
    };

    // Empty string from an unset <select> must become None, not Some("").
    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let default_currency_id = default_currency_id(&state.db).await;

    // Fetch the current row to fill in any fields the form didn't touch.
    let cur: Option<(String, i64, Option<f64>, Option<i64>)> = sqlx::query_as(
        "SELECT date, currency_id, foreign_currency_amount, foreign_currency_id \
         FROM transactions WHERE id = ? AND owner_id = ?",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((cur_date, cur_currency_id, cur_fca, _)) = cur else {
        return Ok(Err("Transaction introuvable ou non autorisée".into()));
    };
    let effective_date        = form.date.clone().unwrap_or(cur_date);
    let effective_currency_id = form.currency_id.unwrap_or(cur_currency_id);
    // A "foreign" transaction is one whose currency differs from the app default.
    let is_foreign            = effective_currency_id != default_currency_id;

    // If the currency is foreign, fetch the exchange rate and convert to the default currency.
    // The original foreign amount is stored separately for display purposes.
    let (final_amount, final_currency_id, final_fca, final_fci): (Option<f64>, i64, Option<f64>, Option<i64>) =
        if is_foreign {
            let foreign_amount = amount.or(cur_fca).unwrap_or(0.0);
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
            (amount, default_currency_id, None, None)
        };

    // One DB transaction: the row, its shares and its status are saved together or not at all.
    // (The exchange rate above is fetched first — no network call while the DB is locked.)
    let mut tx = state.db.begin().await?;

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
    .execute(&mut *tx)
    .await?;

    if let Some(shares) = &shares {
        replace_shares(&mut tx, id, shares).await?;
    }
    let is_complete = recalculate_status(&mut tx, id).await?;
    tx.commit().await?;

    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(id=%id, user_id=%user_id, is_complete=%is_complete, is_foreign=%is_foreign, "Transaction updated");
    Ok(Ok(is_complete))
}

/// POST /transactions/:id/update — save the edit modal (fields and shares). The header
/// shows a toast, closes the modal and fires `transactions-changed`: the whole table
/// re-fetches itself (rows are never swapped one by one).
pub async fn post_update_transaction(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<Response> {
    let user_id = get_user_id(&jar)?;
    if let Err(msg) = update_transaction(&state, user_id, id, &form).await? {
        return Ok(htmx_inline_error("#edit-transaction-error", &msg));
    }
    Ok(toast_response("Transaction enregistrée.", true, &["close-modal", "transactions-changed"]))
}

/// POST /transactions/:id/delete — delete a transaction owned by the current user.
/// Like every write, the answer fires `transactions-changed` so the whole table
/// re-fetches itself; on error the toast shows the reason.
pub async fn post_delete_transaction(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> Response {
    match delete_transaction_inner(&state, &jar, id).await {
        Ok(()) => toast_response("Transaction supprimée.", true, &["close-modal", "transactions-changed"]),
        Err(e) => toast_response(&e.user_message(), false, &["close-modal", "transactions-changed"]),
    }
}

// ── Carousel ("À compléter") ──────────────────────────────────────────────────

/// `?index=` — position of the card in the current user's pending list.
#[derive(Deserialize, Default)]
pub struct CarouselParams {
    pub index: Option<usize>,
}

/// The current user's pending/broken transactions, in carousel order (newest first).
async fn user_pending(db: &DbPool, user_id: i64) -> AppResult<Vec<TransactionView>> {
    Ok(fetch_transaction_views(db, true)
        .await?
        .into_iter()
        .filter(|t| t.owner_id == user_id)
        .collect())
}

/// Render the carousel card for `pending[index]`; `anim` is the CSS entry animation.
async fn render_carousel_card(
    state: &AppState,
    user_id: i64,
    pending: &[TransactionView],
    index: usize,
    anim: &str,
) -> AppResult<Html<String>> {
    let t = &pending[index];
    let mut ctx = tera::Context::new();
    insert_form_options(&mut ctx, &state.db, user_id).await?;
    ctx.insert("t", t);
    ctx.insert("index", &index);
    ctx.insert("count", &pending.len());
    ctx.insert("anim", anim);
    ctx.insert("shares_init", &shares_init(t));
    Ok(Html(state.tera.render("modal/transaction_carousel.html", &ctx)?))
}

/// GET /transactions/pending — open the carousel on its first (or `?index=`) card.
pub async fn get_carousel(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(params): Query<CarouselParams>,
) -> AppResult<Response> {
    let user_id = get_user_id(&jar)?;
    let pending = user_pending(&state.db, user_id).await?;
    if pending.is_empty() {
        return Ok(toast_response("Toutes les transactions sont complètes !", true, &[]));
    }
    let index = params.index.unwrap_or(0).min(pending.len() - 1);
    Ok(render_carousel_card(&state, user_id, &pending, index, "carousel-fade-in").await?.into_response())
}

/// POST /transactions/pending/:id?index= — save the card, then answer with the card to
/// show next (`nav` = prev / next / stay), or an empty body that closes the carousel
/// (`nav` = close, or nothing left to complete). Completed transactions leave the list,
/// so "next" stays on the same index, which now holds the following card.
pub async fn post_carousel(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Query(params): Query<CarouselParams>,
    Form(form): Form<UpdateTransactionForm>,
) -> AppResult<Response> {
    let user_id = get_user_id(&jar)?;
    if let Err(msg) = update_transaction(&state, user_id, id, &form).await? {
        return Ok(htmx_inline_error("#carousel-error", &msg));
    }

    let changed = ["transactions-changed"];
    let pending = user_pending(&state.db, user_id).await?;
    let nav = form.nav.as_deref().unwrap_or("stay");
    if pending.is_empty() {
        return Ok(toast_response("Toutes les transactions sont complètes !", true, &changed));
    }
    if nav == "close" {
        return Ok(toast_response("Transaction enregistrée !", true, &changed));
    }

    let n = pending.len();
    let index = params.index.unwrap_or(0);
    let next = match pending.iter().position(|t| t.id == id) {
        Some(p) => match nav {
            "next" => (p + 1) % n,
            "prev" => (p + n - 1) % n,
            _ => p,
        },
        // The saved card is complete and left the list.
        None => match nav {
            "next" => index % n,
            "prev" => (index + n - 1) % n,
            _ => index.min(n - 1),
        },
    };
    let anim = match nav {
        "next" => "carousel-in-right",
        "prev" => "carousel-in-left",
        _ => "carousel-fade-in",
    };
    let card = render_carousel_card(&state, user_id, &pending, next, anim).await?;
    let trigger = if nav == "stay" {
        toast_header("Transaction enregistrée.", true, &changed)
    } else {
        events_header(&changed)
    };
    Ok(([trigger], card).into_response())
}

async fn delete_transaction_inner(state: &AppState, jar: &SignedCookieJar, id: i64) -> AppResult<()> {
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
    /// JSON-encoded list of `{ user_id, share }` objects (see `modal/_shares_field.html`).
    pub shares_json: Option<String>,
}

/// POST /transactions/manual — create a transaction directly (not from a PDF).
///
/// Manual transactions are linked to a synthetic `imported_files` row with
/// filename = 'manual' so the rest of the codebase can treat them uniformly.
/// That sentinel row is created on demand the first time a manual transaction is added.
///
/// The form uses `hx-swap="none"`; the response's HX-Trigger shows a toast, closes the
/// modal and fires `transactions-changed` so the table re-fetches itself.
pub async fn post_manual_transaction(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<ManualTransactionForm>,
) -> AppResult<Response> {
    let user_id = super::get_user_id(&jar)?;

    // Validate the shares before writing anything.
    let shares = match parse_shares(form.shares_json.as_deref().unwrap_or("[]")) {
        Ok(shares) => shares,
        Err(msg) => return Ok(htmx_inline_error("#manual-transaction-error", &msg)),
    };

    let category_id: Option<i64> = form.category_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());

    let default_currency_id = default_currency_id(&state.db).await;

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

    // One DB transaction for every write below (the exchange rate was fetched above,
    // outside it): the transaction, its shares and its status are saved together.
    let mut tx = state.db.begin().await?;

    // Look up or create the synthetic "manual" import record.
    let file_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM imported_files WHERE filename = 'manual' LIMIT 1"
    )
    .fetch_optional(&mut *tx)
    .await?;

    let file_id = if let Some(id) = file_id {
        id
    } else {
        sqlx::query_scalar(
            "INSERT INTO imported_files (filename, bank_id, source_file, row_count, importer_id) VALUES ('manual', 1, '', 0, ?) RETURNING id",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?
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
    .fetch_one(&mut *tx)
    .await?;

    replace_shares(&mut tx, new_transaction_id, &shares).await?;
    // Now that shares are in place, check if the transaction is immediately complete.
    recalculate_status(&mut tx, new_transaction_id).await?;
    tx.commit().await?;

    // Broadcast to all other connected users so their tables refresh.
    state.events.send(crate::sse::AppEvent { kind: "transactions", actor_id: user_id });
    tracing::info!(user_id=%user_id, transaction_id=%new_transaction_id, is_foreign=%is_foreign, "Manual transaction added");

    Ok(toast_response("Transaction ajoutée.", true, &["close-modal", "transactions-changed"]))
}

/// Recalculate and persist the status of a transaction.
///
/// A transaction is `complete` when:
///   - It has a category set, AND
///   - The sum of all transaction_shares rows equals exactly 100%
///   - It is NOT broken (no parse_note)
///
/// This is called after every edit, share change, or manual transaction creation — on the
/// same DB transaction as the write, so the status always matches the committed data.
async fn recalculate_status(conn: &mut SqliteConnection, transaction_id: i64) -> AppResult<bool> {
    let row: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT category_id, parse_note FROM transactions WHERE id = ?",
    )
    .bind(transaction_id)
    .fetch_optional(&mut *conn)
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
    .fetch_one(&mut *conn)
    .await?;

    // 0.01 tolerance to handle floating-point rounding (e.g. 33.33 + 33.33 + 33.34 = 100.00).
    let is_complete = cat_id.is_some()
        && shares_sum.map(|s| (s - 100.0).abs() < 0.01).unwrap_or(false);

    // status_id 1 = pending, 2 = complete (see transaction_status seed data in migrations).
    let new_status_id: i64 = if is_complete { 2 } else { 1 };

    sqlx::query("UPDATE transactions SET status_id = ? WHERE id = ?")
        .bind(new_status_id)
        .bind(transaction_id)
        .execute(&mut *conn)
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
/// Filtering and sorting happen in Rust afterwards (`filter_and_sort`) — fine for a
/// dataset this small, and it avoids dynamic SQL.
async fn fetch_transaction_views(db: &DbPool, pending_only: bool) -> AppResult<Vec<TransactionView>> {
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

    Ok(raws.into_iter().map(raw_to_view).collect())
}

/// Number of pending/broken transactions owned by `user_id` (the "À compléter" badge).
async fn pending_count(db: &DbPool, user_id: i64) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM transactions t \
         JOIN transaction_status s ON t.status_id = s.id \
         WHERE s.name IN ('pending', 'broken') AND t.owner_id = ?",
    )
    .bind(user_id)
    .fetch_one(db)
    .await?)
}

/// GET /transactions/pending-badge — returns the pending count badge HTML fragment.
/// Re-fetched on SSE / `transactions-changed` so the badge stays current.
pub async fn get_pending_badge(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Html<String>> {
    let user_id = get_user_id(&jar)?;
    let mut ctx = tera::Context::new();
    ctx.insert("pending_count", &pending_count(&state.db, user_id).await?);
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
