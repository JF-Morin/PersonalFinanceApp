use axum::{extract::State, response::{Html, IntoResponse, Json}};
use axum_extra::extract::CookieJar;
use std::collections::HashMap;

use crate::{db::DbPool, error::AppResult, models::{Currency, UserPairBalance}, AppState};
use super::base_context;

// ── Shared computation helpers ──────────────────────────────────────────────

async fn compute_balances(db: &DbPool) -> AppResult<Vec<UserPairBalance>> {
    // Fetch all complete transactions that involve sharing (other_share set)
    let rows: Vec<(i64, f64, Option<f64>, i64)> = sqlx::query_as(
        r#"
        SELECT t.paid_by, t.amount, t.other_share, t.currency_id
        FROM transactions t
        JOIN transaction_status s ON t.status_id = s.id
        WHERE s.name = 'complete' AND t.other_share IS NOT NULL
        "#
    )
    .fetch_all(db)
    .await?;

    let all_users = sqlx::query_as::<_, crate::models::User>("SELECT * FROM users ORDER BY id")
        .fetch_all(db)
        .await?;

    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(db)
        .await?;
    let currency_symbols: HashMap<i64, String> =
        currencies.iter().map(|c| (c.id, c.symbol.clone())).collect();
    let user_names: HashMap<i64, String> =
        all_users.iter().map(|u| (u.id, u.display_name())).collect();

    // For each (user_a, user_b, currency) pair where a < b, accumulate:
    //   paid_for_self[uid][cid]  = amount the user kept for themselves
    //   paid_for_other[uid][cid] = amount the user covered for the other
    //   paid_total[uid][cid]     = |amount| total
    let mut paid_for_self:  HashMap<(i64, i64), f64> = HashMap::new();
    let mut paid_for_other: HashMap<(i64, i64), f64> = HashMap::new();
    let mut paid_total:     HashMap<(i64, i64), f64> = HashMap::new();

    for (paid_by, amount, other_share, currency_id) in &rows {
        let abs_amount = amount.abs();
        let other_frac  = other_share.unwrap_or(0.0) / 100.0;
        let self_frac   = 1.0 - other_frac;

        let for_other = abs_amount * other_frac;
        let for_self  = abs_amount * self_frac;

        *paid_for_other.entry((*paid_by, *currency_id)).or_insert(0.0) += for_other;
        *paid_for_self .entry((*paid_by, *currency_id)).or_insert(0.0) += for_self;
        *paid_total    .entry((*paid_by, *currency_id)).or_insert(0.0) += abs_amount;
    }

    let user_ids: Vec<i64> = all_users.iter().map(|u| u.id).collect();
    let mut result: Vec<UserPairBalance> = Vec::new();

    // Single-user DB: no second user exists yet — show balance against a placeholder.
    if user_ids.len() == 1 {
        let uid1 = user_ids[0];
        let uname1 = user_names.get(&uid1).cloned().unwrap_or_default();
        let placeholder = "Autre personne".to_string();

        let cids: std::collections::HashSet<i64> = paid_total.keys()
            .filter(|&&(uid, _)| uid == uid1)
            .map(|&(_, cid)| cid)
            .collect();

        for cid in cids {
            let u1_total     = paid_total    .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_for_self  = paid_for_self .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_for_other = paid_for_other.get(&(uid1, cid)).copied().unwrap_or(0.0);
            if u1_total < 0.005 { continue; }

            let net_amount = (u1_for_other * 100.0).round() / 100.0;
            let sym = currency_symbols.get(&cid).cloned().unwrap_or_default();
            result.push(UserPairBalance {
                user1_name:           uname1.clone(),
                user1_paid_total:     (u1_total    * 100.0).round() / 100.0,
                user1_paid_for_self:  (u1_for_self * 100.0).round() / 100.0,
                user1_paid_for_other: (u1_for_other* 100.0).round() / 100.0,
                user2_name:           placeholder.clone(),
                user2_paid_total:     0.0,
                user2_paid_for_self:  0.0,
                user2_paid_for_other: 0.0,
                debtor:   if net_amount > 0.005 { placeholder.clone() } else { String::new() },
                creditor: if net_amount > 0.005 { uname1.clone() } else { String::new() },
                net_amount,
                currency_symbol: sym,
            });
        }
        return Ok(result);
    }

    // Multi-user case: one card per (user1, user2, currency) pair.
    for i in 0..user_ids.len() {
        for j in (i + 1)..user_ids.len() {
            let uid1 = user_ids[i];
            let uid2 = user_ids[j];

            let mut relevant_currencies: std::collections::HashSet<i64> = std::collections::HashSet::new();
            for &(uid, cid) in paid_total.keys() {
                if uid == uid1 || uid == uid2 {
                    relevant_currencies.insert(cid);
                }
            }

            for cid in relevant_currencies {
                let u1_total     = paid_total    .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_for_self  = paid_for_self .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_for_other = paid_for_other.get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u2_total     = paid_total    .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_for_self  = paid_for_self .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_for_other = paid_for_other.get(&(uid2, cid)).copied().unwrap_or(0.0);

                if u1_total < 0.005 && u2_total < 0.005 {
                    continue;
                }

                // Net: user1 covered for user2, user2 covered for user1.
                // If u1_for_other > u2_for_other → user2 owes user1 the difference.
                let net = u1_for_other - u2_for_other;
                let (debtor, creditor, net_amount) = if net > 0.005 {
                    (
                        user_names.get(&uid2).cloned().unwrap_or_default(),
                        user_names.get(&uid1).cloned().unwrap_or_default(),
                        net,
                    )
                } else if net < -0.005 {
                    (
                        user_names.get(&uid1).cloned().unwrap_or_default(),
                        user_names.get(&uid2).cloned().unwrap_or_default(),
                        -net,
                    )
                } else {
                    (String::new(), String::new(), 0.0)
                };

                let sym = currency_symbols.get(&cid).cloned().unwrap_or_default();
                result.push(UserPairBalance {
                    user1_name:           user_names.get(&uid1).cloned().unwrap_or_default(),
                    user1_paid_total:     (u1_total    * 100.0).round() / 100.0,
                    user1_paid_for_self:  (u1_for_self * 100.0).round() / 100.0,
                    user1_paid_for_other: (u1_for_other* 100.0).round() / 100.0,
                    user2_name:           user_names.get(&uid2).cloned().unwrap_or_default(),
                    user2_paid_total:     (u2_total    * 100.0).round() / 100.0,
                    user2_paid_for_self:  (u2_for_self * 100.0).round() / 100.0,
                    user2_paid_for_other: (u2_for_other* 100.0).round() / 100.0,
                    debtor,
                    creditor,
                    net_amount: (net_amount * 100.0).round() / 100.0,
                    currency_symbol: sym,
                });
            }
        }
    }
    Ok(result)
}

async fn compute_chart_data(db: &DbPool) -> AppResult<(Vec<String>, Vec<f64>)> {
    let by_category: Vec<(Option<String>, Option<f64>)> = sqlx::query_as(
        r#"
        SELECT COALESCE(c.name, 'Non catégorisé') AS category, SUM(ABS(t.amount)) AS total
        FROM transactions t
        LEFT JOIN categories c ON t.category_id = c.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE t.amount != 0 AND s.name = 'complete'
        GROUP BY c.name
        ORDER BY total DESC
        LIMIT 10
        "#
    )
    .fetch_all(db)
    .await?;

    let labels = by_category
        .iter()
        .map(|(cat, _)| cat.clone().unwrap_or_else(|| "Non catégorisé".into()))
        .collect();
    let totals = by_category
        .iter()
        .map(|(_, tot)| tot.unwrap_or(0.0))
        .collect();
    Ok((labels, totals))
}

// ── Full home page ──────────────────────────────────────────────────────────

pub async fn get_home(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    let pair_balances = compute_balances(&state.db).await?;
    let (category_labels, category_totals) = compute_chart_data(&state.db).await?;

    let total_transactions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transactions")
        .fetch_one(&state.db)
        .await?;
    let total_files: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM imported_files WHERE filename != 'manual'"
    )
    .fetch_one(&state.db)
    .await?;
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM transactions t \
         JOIN transaction_status s ON t.status_id = s.id \
         WHERE s.name IN ('pending', 'broken')"
    )
    .fetch_one(&state.db)
    .await?;

    ctx.insert("pair_balances", &pair_balances);
    ctx.insert("pending_count", &pending_count);
    ctx.insert("total_transactions", &total_transactions);
    ctx.insert("total_files", &total_files);
    ctx.insert("category_labels", &category_labels);
    ctx.insert("category_totals", &category_totals);

    let html = state.tera.render("home.html", &ctx)?;
    Ok(Html(html))
}

// ── Partial: balances section (HTMX swap) ──────────────────────────────────

pub async fn get_balances_partial(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    super::get_user_id(&jar)?; // auth guard
    let pair_balances = compute_balances(&state.db).await?;
    let mut ctx = tera::Context::new();
    ctx.insert("pair_balances", &pair_balances);
    let html = state.tera.render("home/_balances.html", &ctx)?;
    Ok(Html(html))
}

// ── Partial: chart data (JSON, consumed by JS) ─────────────────────────────

pub async fn get_chart_data(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    super::get_user_id(&jar)?;
    let (labels, totals) = compute_chart_data(&state.db).await?;
    Ok(Json(serde_json::json!({ "labels": labels, "totals": totals })))
}
