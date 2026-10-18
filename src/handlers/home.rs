use axum::{extract::State, response::Html};
use axum_extra::extract::CookieJar;

use crate::{error::AppResult, models::Balance, AppState};
use super::base_context;

pub async fn get_home(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    // Balances: for each complete transaction with other_share set,
    // compute what each user owes each other (per currency).
    let rows = sqlx::query!(
        r#"
        SELECT t.paid_by, t.amount, t.other_share, t.currency_id,
               u.name || COALESCE(' ' || u.last_name, '') AS payer_name,
               c.symbol AS currency_symbol
        FROM transactions t
        JOIN users u ON t.paid_by = u.id
        JOIN currencies c ON t.currency_id = c.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE s.name = 'complete' AND t.other_share IS NOT NULL AND t.other_share > 0
        "#
    )
    .fetch_all(&state.db)
    .await?;

    let all_users = sqlx::query_as::<_, crate::models::User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    // Simple two-user balance aggregation per currency
    use std::collections::HashMap;
    let mut debts: HashMap<(i64, i64, i64), f64> = HashMap::new(); // (from_id, to_id, currency_id) -> amount

    for row in &rows {
        let share = row.other_share.unwrap_or(0.0) / 100.0;
        let owed = row.amount.abs() * share;

        // Find the "other" user (everyone except payer)
        for other in &all_users {
            if other.id != row.paid_by {
                let key = (other.id, row.paid_by, row.currency_id);
                *debts.entry(key).or_insert(0.0) += owed;
            }
        }
    }

    let mut balances: Vec<Balance> = Vec::new();
    let currency_symbols: HashMap<i64, String> = rows
        .iter()
        .map(|r| (r.currency_id, r.currency_symbol.clone()))
        .collect();

    let user_names: HashMap<i64, String> = all_users
        .iter()
        .map(|u| (u.id, u.display_name()))
        .collect();

    for ((from_id, to_id, currency_id), amount) in debts {
        if amount.abs() > 0.005 {
            balances.push(Balance {
                from_user: user_names.get(&from_id).cloned().unwrap_or_default(),
                to_user: user_names.get(&to_id).cloned().unwrap_or_default(),
                amount: (amount * 100.0).round() / 100.0,
                currency_symbol: currency_symbols.get(&currency_id).cloned().unwrap_or_default(),
            });
        }
    }

    let pending_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM transactions t JOIN transaction_status s ON t.status_id = s.id WHERE s.name = 'pending'"
    )
    .fetch_one(&state.db)
    .await?;

    ctx.insert("balances", &balances);
    ctx.insert("pending_count", &pending_count);

    let html = state.tera.render("home.html", &ctx)?;
    Ok(Html(html))
}
