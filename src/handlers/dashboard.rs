use axum::{extract::State, response::Html};
use axum_extra::extract::CookieJar;

use crate::{error::AppResult, AppState};
use super::base_context;

pub async fn get_dashboard(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    let total_transactions = sqlx::query_scalar!("SELECT COUNT(*) FROM transactions")
        .fetch_one(&state.db)
        .await?;

    let total_files = sqlx::query_scalar!("SELECT COUNT(*) FROM imported_files")
        .fetch_one(&state.db)
        .await?;

    let pending_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM transactions t JOIN transaction_status s ON t.status_id = s.id WHERE s.name = 'pending'"
    )
    .fetch_one(&state.db)
    .await?;

    // Spending by category (debit only, complete transactions)
    let by_category = sqlx::query!(
        r#"
        SELECT COALESCE(c.name, 'Non catégorisé') AS category, SUM(ABS(t.amount)) AS total
        FROM transactions t
        LEFT JOIN categories c ON t.category_id = c.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE t.amount < 0 AND s.name = 'complete'
        GROUP BY c.name
        ORDER BY total DESC
        LIMIT 10
        "#
    )
    .fetch_all(&state.db)
    .await?;

    let category_labels: Vec<String> = by_category.iter()
        .map(|r| r.category.clone().unwrap_or_else(|| "Non catégorisé".into()))
        .collect();
    let category_totals: Vec<f64> = by_category.iter()
        .map(|r| r.total.unwrap_or(0.0))
        .collect();

    ctx.insert("total_transactions", &total_transactions);
    ctx.insert("total_files", &total_files);
    ctx.insert("pending_count", &pending_count);
    ctx.insert("category_labels", &category_labels);
    ctx.insert("category_totals", &category_totals);

    let html = state.tera.render("dashboard.html", &ctx)?;
    Ok(Html(html))
}
