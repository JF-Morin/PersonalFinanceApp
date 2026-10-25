use axum::{extract::State, response::{Html, IntoResponse, Json}};
use axum_extra::extract::CookieJar;
use std::collections::HashMap;

use crate::{db::DbPool, error::AppResult, models::{Currency, UserPairBalance}, AppState};
use super::base_context;

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

async fn compute_balances(db: &DbPool) -> AppResult<Vec<UserPairBalance>> {
    // Query 1: all complete transactions — owner pays/receives the full amount.
    let owner_rows: Vec<(i64, i64, f64)> = sqlx::query_as(
        r#"
        SELECT t.owner_id, t.currency_id, t.amount
        FROM transactions t
        JOIN transaction_status s ON t.status_id = s.id
        WHERE s.name = 'complete'
        "#
    )
    .fetch_all(db)
    .await?;

    // Query 2: non-owner shares — what each participant owes (or is owed by) the owner.
    let share_rows: Vec<(i64, i64, i64, f64, f64)> = sqlx::query_as(
        r#"
        SELECT t.owner_id, ts.user_id, t.currency_id, t.amount, ts.share
        FROM transaction_shares ts
        JOIN transactions t ON ts.transaction_id = t.id
        JOIN transaction_status s ON t.status_id = s.id
        WHERE s.name = 'complete' AND ts.user_id != t.owner_id
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

    // Per-owner totals: the full transaction amounts (not share-weighted).
    // Key: (owner_id, currency_id)
    let mut total_debit:  HashMap<(i64, i64), f64> = HashMap::new();
    let mut total_credit: HashMap<(i64, i64), f64> = HashMap::new();

    for (owner_id, currency_id, amount) in &owner_rows {
        if *amount < 0.0 {
            *total_debit.entry((*owner_id, *currency_id)).or_insert(0.0) += amount;
        } else {
            *total_credit.entry((*owner_id, *currency_id)).or_insert(0.0) += amount;
        }
    }

    // Cross-user obligations: amount * share% that the non-owner participant owes the owner.
    // Negative portion = participant owes owner (expense); positive = owner owes participant (income).
    // Key: (owner_id, other_user_id, currency_id)
    let mut debit_for_other:  HashMap<(i64, i64, i64), f64> = HashMap::new();
    let mut credit_for_other: HashMap<(i64, i64, i64), f64> = HashMap::new();

    for (owner_id, user_id, currency_id, amount, share) in &share_rows {
        let portion = amount * share / 100.0;
        if *amount < 0.0 {
            *debit_for_other.entry((*owner_id, *user_id, *currency_id)).or_insert(0.0) += portion;
        } else {
            *credit_for_other.entry((*owner_id, *user_id, *currency_id)).or_insert(0.0) += portion;
        }
    }

    let user_ids: Vec<i64> = all_users.iter().map(|u| u.id).collect();
    let mut result: Vec<UserPairBalance> = Vec::new();

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
            let u1_td  = total_debit .get(&(uid1, cid)).copied().unwrap_or(0.0);
            let u1_tc  = total_credit.get(&(uid1, cid)).copied().unwrap_or(0.0);

            if u1_td.abs() < 0.005 && u1_tc < 0.005 { continue; }

            let sym = currency_symbols.get(&cid).cloned().unwrap_or_default();
            result.push(UserPairBalance {
                user1_name:             uname1.clone(),
                user1_total_debit:      round2(u1_td),
                user1_total_credit:     round2(u1_tc),
                user1_debit_for_other:  0.0,
                user1_credit_for_other: 0.0,
                user1_total_for_other:  0.0,
                user2_name:             placeholder.clone(),
                user2_total_debit:      0.0,
                user2_total_credit:     0.0,
                user2_debit_for_other:  0.0,
                user2_credit_for_other: 0.0,
                user2_total_for_other:  0.0,
                debtor:     String::new(),
                creditor:   String::new(),
                net_amount: 0.0,
                currency_symbol: sym,
            });
        }
        return Ok(result);
    }

    for i in 0..user_ids.len() {
        for j in (i + 1)..user_ids.len() {
            let uid1 = user_ids[i];
            let uid2 = user_ids[j];

            let mut cids: std::collections::HashSet<i64> = std::collections::HashSet::new();
            for &(uid, cid) in total_debit.keys().chain(total_credit.keys()) {
                if uid == uid1 || uid == uid2 { cids.insert(cid); }
            }

            for cid in cids {
                let u1_td  = total_debit .get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_tc  = total_credit.get(&(uid1, cid)).copied().unwrap_or(0.0);
                let u1_dfo = debit_for_other .get(&(uid1, uid2, cid)).copied().unwrap_or(0.0);
                let u1_cfo = credit_for_other.get(&(uid1, uid2, cid)).copied().unwrap_or(0.0);
                let u1_tfo = u1_dfo + u1_cfo;

                let u2_td  = total_debit .get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_tc  = total_credit.get(&(uid2, cid)).copied().unwrap_or(0.0);
                let u2_dfo = debit_for_other .get(&(uid2, uid1, cid)).copied().unwrap_or(0.0);
                let u2_cfo = credit_for_other.get(&(uid2, uid1, cid)).copied().unwrap_or(0.0);
                let u2_tfo = u2_dfo + u2_cfo;

                if u1_td.abs() < 0.005 && u1_tc < 0.005 && u2_td.abs() < 0.005 && u2_tc < 0.005 {
                    continue;
                }

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

pub async fn get_balances_partial(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    super::get_user_id(&jar)?;
    let pair_balances = compute_balances(&state.db).await?;
    let mut ctx = tera::Context::new();
    ctx.insert("pair_balances", &pair_balances);
    let html = state.tera.render("home/_balances.html", &ctx)?;
    Ok(Html(html))
}

pub async fn get_chart_data(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    super::get_user_id(&jar)?;
    let (labels, totals) = compute_chart_data(&state.db).await?;
    Ok(Json(serde_json::json!({ "labels": labels, "totals": totals })))
}

pub async fn get_stats(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    super::get_user_id(&jar)?;
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
    Ok(Json(serde_json::json!({
        "total_transactions": total_transactions,
        "total_files": total_files,
        "pending_count": pending_count,
    })))
}
