use axum::{extract::State, response::{Html, IntoResponse, Json}};
use axum_extra::extract::CookieJar;
use std::collections::HashMap;

use crate::{db::DbPool, error::AppResult, models::{Currency, UserPairBalance}, AppState};
use super::base_context;

// ── Shared computation helpers ──────────────────────────────────────────────

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

async fn compute_balances(db: &DbPool) -> AppResult<Vec<UserPairBalance>> {
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

    // Accumulate signed per-user per-currency values.
    // debit = negative amounts, credit = positive amounts.
    let mut total_debit:      HashMap<(i64, i64), f64> = HashMap::new();
    let mut total_credit:     HashMap<(i64, i64), f64> = HashMap::new();
    let mut debit_for_other:  HashMap<(i64, i64), f64> = HashMap::new();
    let mut credit_for_other: HashMap<(i64, i64), f64> = HashMap::new();

    for (paid_by, amount, other_share, currency_id) in &rows {
        let share = other_share.unwrap_or(0.0) / 100.0;
        if *amount < 0.0 {
            *total_debit    .entry((*paid_by, *currency_id)).or_insert(0.0) += amount;
            *debit_for_other.entry((*paid_by, *currency_id)).or_insert(0.0) += amount * share;
        } else {
            *total_credit    .entry((*paid_by, *currency_id)).or_insert(0.0) += amount;
            *credit_for_other.entry((*paid_by, *currency_id)).or_insert(0.0) += amount * share;
        }
    }

    let user_ids: Vec<i64> = all_users.iter().map(|u| u.id).collect();
    let mut result: Vec<UserPairBalance> = Vec::new();

    // Single-user DB: show balance against a placeholder.
    if user_ids.len() == 1 {
        let uid1 = user_ids[0];
        let uname1 = user_names.get(&uid1).cloned().unwrap_or_default();
        let placeholder = "Autre personne".to_string();

        let cids: std::collections::HashSet<i64> = total_debit.keys()
            .chain(total_credit.keys())
            .filter(|&&(uid, _)| uid == uid1)
            .map(|&(_, cid)| cid)
            .collect();

        for cid in cids {
            let u1_td  = total_debit    .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_tc  = total_credit   .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_dfo = debit_for_other .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_cfo = credit_for_other.get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_tfo = u1_dfo + u1_cfo;

            if u1_td.abs() < 0.005 && u1_tc < 0.005 { continue; }

            // net = u1_tfo - 0: negative → placeholder owes u1; positive → u1 owes placeholder
            let (debtor, creditor, net_amount) = if u1_tfo < -0.005 {
                (placeholder.clone(), uname1.clone(), -u1_tfo)
            } else if u1_tfo > 0.005 {
                (uname1.clone(), placeholder.clone(), u1_tfo)
            } else {
                (String::new(), String::new(), 0.0)
            };

            let sym = currency_symbols.get(&cid).cloned().unwrap_or_default();
            result.push(UserPairBalance {
                user1_name:             uname1.clone(),
                user1_total_debit:      round2(u1_td),
                user1_total_credit:     round2(u1_tc),
                user1_debit_for_other:  round2(u1_dfo),
                user1_credit_for_other: round2(u1_cfo),
                user1_total_for_other:  round2(u1_tfo),
                user2_name:             placeholder.clone(),
                user2_total_debit:      0.0,
                user2_total_credit:     0.0,
                user2_debit_for_other:  0.0,
                user2_credit_for_other: 0.0,
                user2_total_for_other:  0.0,
                debtor,
                creditor,
                net_amount: round2(net_amount),
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

            let mut cids: std::collections::HashSet<i64> = std::collections::HashSet::new();
            for &(uid, cid) in total_debit.keys().chain(total_credit.keys()) {
                if uid == uid1 || uid == uid2 { cids.insert(cid); }
            }

            for cid in cids {
                let u1_td  = total_debit    .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_tc  = total_credit   .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_dfo = debit_for_other .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_cfo = credit_for_other.get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_tfo = u1_dfo + u1_cfo;

                let u2_td  = total_debit    .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_tc  = total_credit   .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_dfo = debit_for_other .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_cfo = credit_for_other.get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_tfo = u2_dfo + u2_cfo;

                if u1_td.abs() < 0.005 && u1_tc < 0.005 && u2_td.abs() < 0.005 && u2_tc < 0.005 {
                    continue;
                }

                // net = u1_tfo - u2_tfo
                // positive → u1 owes u2; negative → u2 owes u1
                let net = u1_tfo - u2_tfo;
                let (debtor, creditor, net_amount) = if net > 0.005 {
                    (user_names.get(&uid1).cloned().unwrap_or_default(),
                     user_names.get(&uid2).cloned().unwrap_or_default(), net)
                } else if net < -0.005 {
                    (user_names.get(&uid2).cloned().unwrap_or_default(),
                     user_names.get(&uid1).cloned().unwrap_or_default(), -net)
                } else {
                    (String::new(), String::new(), 0.0)
                };

                let sym = currency_symbols.get(&cid).cloned().unwrap_or_default();
                result.push(UserPairBalance {
                    user1_name:             user_names.get(&uid1).cloned().unwrap_or_default(),
                    user1_total_debit:      round2(u1_td),
                    user1_total_credit:     round2(u1_tc),
                    user1_debit_for_other:  round2(u1_dfo),
                    user1_credit_for_other: round2(u1_cfo),
                    user1_total_for_other:  round2(u1_tfo),
                    user2_name:             user_names.get(&uid2).cloned().unwrap_or_default(),
                    user2_total_debit:      round2(u2_td),
                    user2_total_credit:     round2(u2_tc),
                    user2_debit_for_other:  round2(u2_dfo),
                    user2_credit_for_other: round2(u2_cfo),
                    user2_total_for_other:  round2(u2_tfo),
                    debtor,
                    creditor,
                    net_amount: round2(net_amount),
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
