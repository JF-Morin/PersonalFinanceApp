use axum::{extract::State, response::Html};
use axum_extra::extract::CookieJar;

use crate::{db::get_setting, error::AppResult, models::{Balance, Currency}, AppState};
use super::base_context;

pub async fn get_home(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    // Balances: for each complete transaction with other_share set,
    // compute what each user owes each other (per currency).
    let rows: Vec<(i64, f64, Option<f64>, i64, String, String)> = sqlx::query_as(
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

    for (paid_by, amount, other_share, currency_id, _, _) in &rows {
        let share = other_share.unwrap_or(0.0) / 100.0;
        let owed = amount.abs() * share;

        // Assumes two users: "the other" is anyone who didn't pay.
        // With more users this would split the share equally, which may not be desired.
        for other in &all_users {
            if other.id != *paid_by {
                let key = (other.id, *paid_by, *currency_id);
                *debts.entry(key).or_insert(0.0) += owed;
            }
        }
    }

    let mut balances: Vec<Balance> = Vec::new();
    let currency_symbols: HashMap<i64, String> = rows
        .iter()
        .map(|(_, _, _, cid, _, sym)| (*cid, sym.clone()))
        .collect();

    let user_names: HashMap<i64, String> = all_users
        .iter()
        .map(|u| (u.id, u.display_name()))
        .collect();

    for ((from_id, to_id, currency_id), amount) in debts {
        if amount.abs() > 0.005 { // skip sub-cent rounding noise
            balances.push(Balance {
                from_user: user_names.get(&from_id).cloned().unwrap_or_default(),
                to_user: user_names.get(&to_id).cloned().unwrap_or_default(),
                amount: (amount * 100.0).round() / 100.0,
                currency_symbol: currency_symbols.get(&currency_id).cloned().unwrap_or_default(),
            });
        }
    }

    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM transactions t JOIN transaction_status s ON t.status_id = s.id WHERE s.name = 'pending'"
    )
    .fetch_one(&state.db)
    .await?;

    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let default_currency_id: i64 = get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    ctx.insert("balances", &balances);
    ctx.insert("pending_count", &pending_count);
    ctx.insert("currencies", &currencies);
    ctx.insert("default_currency_id", &default_currency_id);

    let html = state.tera.render("home.html", &ctx)?;
    Ok(Html(html))
}
