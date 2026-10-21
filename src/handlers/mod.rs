pub mod auth;
pub mod home;
pub mod imports;
pub mod settings;
pub mod transactions;

use axum::http::HeaderMap;
use axum::response::IntoResponse;
use crate::db::DbPool;
use crate::error::AppError;
use crate::models::User;
use axum_extra::extract::CookieJar;

pub fn get_user_id(jar: &CookieJar) -> Result<i64, AppError> {
    jar.get("user_id")
        .and_then(|c| c.value().parse::<i64>().ok())
        .ok_or(AppError::Unauthorized)
}

pub async fn get_current_user(jar: &CookieJar, db: &DbPool) -> Result<User, AppError> {
    let user_id = get_user_id(jar)?;
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::Unauthorized)
}

pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// True when the request was fired by hx-boost (full-page navigation, not a partial swap).
/// Boosted requests carry HX-Boosted: true alongside HX-Request: true.
pub fn is_htmx_boost(headers: &HeaderMap) -> bool {
    headers.get("hx-boosted")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Return an HTMX client-side redirect. Use instead of `Redirect` when the
/// request comes from HTMX (a normal 3xx redirect is ignored by HTMX).
pub fn htmx_redirect(url: &str) -> impl IntoResponse {
    ([(axum::http::header::HeaderName::from_static("hx-redirect"), url.to_string())], "")
}

/// Build a base Tera context with current_user and theme already populated.
pub async fn base_context(jar: &CookieJar, db: &DbPool) -> Result<tera::Context, AppError> {
    let user = get_current_user(jar, db).await?;
    let theme = crate::db::get_setting(db, "theme")
        .await
        .unwrap_or_else(|_| "light".to_string());

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &user);
    ctx.insert("theme", &theme);
    ctx.insert("is_dark", &(theme == "dark"));
    Ok(ctx)
}
