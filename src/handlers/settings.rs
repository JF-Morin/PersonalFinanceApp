use axum::{extract::State, response::{Html, IntoResponse, Redirect}, Form};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::{db::{get_setting, set_setting}, error::AppResult, models::{Bank, Currency, User}, AppState};
use super::base_context;

pub async fn get_settings(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<Html<String>> {
    let mut ctx = base_context(&jar, &state.db).await?;

    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    let default_currency_id: i64 = get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    ctx.insert("currencies", &currencies);
    ctx.insert("banks", &banks);
    ctx.insert("users", &users);
    ctx.insert("default_currency_id", &default_currency_id);

    let html = state.tera.render("settings.html", &ctx)?;
    Ok(Html(html))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub theme: String,
    pub default_currency_id: String,
}

pub async fn post_settings(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<SettingsForm>,
) -> AppResult<impl IntoResponse> {
    let _ = super::get_user_id(&jar)?;

    let theme = if form.theme == "dark" { "dark" } else { "light" };
    set_setting(&state.db, "theme", theme).await?;
    set_setting(&state.db, "default_currency_id", &form.default_currency_id).await?;

    Ok(Redirect::to("/settings"))
}

#[derive(Deserialize)]
pub struct NewUserForm {
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
}

pub async fn post_new_user(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<NewUserForm>,
) -> AppResult<impl IntoResponse> {
    let _ = super::get_user_id(&jar)?;

    sqlx::query("INSERT INTO users (name, last_name, bank_id) VALUES (?, ?, ?)")
        .bind(&form.name)
        .bind(&form.last_name)
        .bind(form.bank_id)
        .execute(&state.db)
        .await?;

    Ok(Redirect::to("/settings"))
}
