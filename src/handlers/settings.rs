use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};

use crate::{db::{get_setting, set_setting}, error::{AppError, AppResult}, models::{Bank, Currency, User}, AppState};
use super::{is_htmx, htmx_redirect};

#[derive(Deserialize, Default)]
pub struct SettingsQuery {
    pub pw_changed: Option<String>,
    pub pw_error: Option<String>,
    pub user_error: Option<String>,
}

pub async fn get_settings(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let mut ctx = super::base_context(&jar, &state.db).await?;

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
    ctx.insert("pw_changed", &query.pw_changed.is_some());
    ctx.insert("pw_error", &query.pw_error.as_deref().unwrap_or(""));
    ctx.insert("user_error", &query.user_error.as_deref().unwrap_or(""));

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
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> AppResult<impl IntoResponse> {
    super::get_user_id(&jar)?;
    let theme = if form.theme == "dark" { "dark" } else { "light" };
    set_setting(&state.db, "theme", theme).await?;
    set_setting(&state.db, "default_currency_id", &form.default_currency_id).await?;

    if is_htmx(&headers) {
        Ok("".into_response())
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    pub old_password: String,
    pub new_password: String,
    pub new_password_confirm: String,
}

pub async fn post_change_password(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<ChangePasswordForm>,
) -> AppResult<impl IntoResponse> {
    let user_id = super::get_user_id(&jar)
        .map_err(|_| crate::error::AppError::Unauthorized)?;

    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| crate::error::AppError::Other("Utilisateur introuvable".into()))?;

    let stored = user.password_hash.as_deref().unwrap_or("");
    let parsed = PasswordHash::new(stored)
        .map_err(|e| crate::error::AppError::Other(e.to_string()))?;

    if Argon2::default().verify_password(form.old_password.as_bytes(), &parsed).is_err() {
        tracing::warn!(user_id = %user_id, "Password change: wrong old password");
        return Ok(Redirect::to("/settings?pw_error=wrong").into_response());
    }

    if form.new_password.is_empty() {
        return Ok(Redirect::to("/settings?pw_error=empty").into_response());
    }

    if form.new_password != form.new_password_confirm {
        return Ok(Redirect::to("/settings?pw_error=mismatch").into_response());
    }

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(form.new_password.as_bytes(), &salt)
        .map_err(|e| crate::error::AppError::Other(e.to_string()))?
        .to_string();

    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hash)
        .bind(user_id)
        .execute(&state.db)
        .await?;

    tracing::info!(user_id = %user_id, "Password changed successfully");
    Ok(Redirect::to("/settings?pw_changed=1").into_response())
}

#[derive(Deserialize)]
pub struct NewUserForm {
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
    pub password: Option<String>,
    pub redirect_to: Option<String>,
}

pub async fn post_new_user(
    State(state): State<AppState>,
    _jar: CookieJar,
    Form(form): Form<NewUserForm>,
) -> AppResult<impl IntoResponse> {
    let duplicate: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE LOWER(name) = LOWER(?)"
    )
    .bind(&form.name)
    .fetch_optional(&state.db)
    .await?;

    if duplicate.is_some() {
        let redirect = match form.redirect_to.as_deref() {
            Some("/login") => "/login?user_error=duplicate",
            _ => "/settings?user_error=duplicate",
        };
        return Ok(Redirect::to(redirect).into_response());
    }

    let password_hash = match form.password.as_deref() {
        Some(pw) if !pw.is_empty() => {
            let salt = SaltString::generate(&mut OsRng);
            let hash = Argon2::default()
                .hash_password(pw.as_bytes(), &salt)
                .map_err(|e| AppError::Other(e.to_string()))?
                .to_string();
            Some(hash)
        }
        _ => None,
    };

    sqlx::query("INSERT INTO users (name, last_name, bank_id, password_hash) VALUES (?, ?, ?, ?)")
        .bind(&form.name)
        .bind(&form.last_name)
        .bind(form.bank_id)
        .bind(&password_hash)
        .execute(&state.db)
        .await?;

    let redirect = match form.redirect_to.as_deref() {
        Some("/login") => "/login",
        _ => "/settings",
    };
    Ok(Redirect::to(redirect).into_response())
}

#[derive(Deserialize)]
pub struct EditUserForm {
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
    pub old_password: Option<String>,
    pub new_password: Option<String>,
    pub new_password_confirm: Option<String>,
}

pub async fn post_edit_user(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
    Form(form): Form<EditUserForm>,
) -> AppResult<impl IntoResponse> {
    super::get_user_id(&jar)?;
    let last_name = form.last_name.as_deref().filter(|s| !s.is_empty());
    sqlx::query("UPDATE users SET name = ?, last_name = ?, bank_id = ? WHERE id = ?")
        .bind(&form.name)
        .bind(last_name)
        .bind(form.bank_id)
        .bind(id)
        .execute(&state.db)
        .await?;

    if let Some(new_pw) = form.new_password.as_deref().filter(|s| !s.is_empty()) {
        let confirm = form.new_password_confirm.as_deref().unwrap_or("");
        if new_pw != confirm {
            return Ok(Redirect::to("/settings?pw_error=mismatch").into_response());
        }

        // Verify old password only if the user already has one set.
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| crate::error::AppError::Other("Utilisateur introuvable".into()))?;

        if let Some(stored) = user.password_hash.as_deref() {
            let old_pw = form.old_password.as_deref().unwrap_or("");
            let parsed = PasswordHash::new(stored)
                .map_err(|e| crate::error::AppError::Other(e.to_string()))?;
            if Argon2::default().verify_password(old_pw.as_bytes(), &parsed).is_err() {
                tracing::warn!(user_id = %id, "Edit modal: wrong old password");
                return Ok(Redirect::to("/settings?pw_error=wrong").into_response());
            }
        }

        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(new_pw.as_bytes(), &salt)
            .map_err(|e| crate::error::AppError::Other(e.to_string()))?
            .to_string();
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(&hash)
            .bind(id)
            .execute(&state.db)
            .await?;
        tracing::info!(user_id = %id, "Password updated via edit modal");
    }

    tracing::info!(user_id = %id, "User profile updated");
    Ok(Redirect::to("/settings").into_response())
}

pub async fn delete_user(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    let current_user_id = super::get_user_id(&jar).ok();
    let is_self = current_user_id.map_or(false, |uid| uid == id);

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;

    tracing::info!(user_id = %id, self_delete = %is_self, "User deleted");

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await?;

    // Log out when the user deletes themselves, or when no users remain.
    if is_self || remaining == 0 {
        let new_jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
        return if is_htmx(&headers) {
            Ok((new_jar, htmx_redirect("/login")).into_response())
        } else {
            Ok((new_jar, Redirect::to("/login")).into_response())
        };
    }

    if is_htmx(&headers) {
        Ok("".into_response())
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}
