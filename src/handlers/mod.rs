pub mod auth;
pub mod home;
pub mod imports;
pub mod settings;
pub mod transactions;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use crate::db::DbPool;
use crate::error::AppError;
use crate::models::User;
use crate::AppState;
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

/// Dedicated 404 page at /404-not-found.
pub async fn get_not_found(State(state): State<AppState>) -> impl IntoResponse {
    let html = state
        .tera
        .render("404.html", &tera::Context::new())
        .unwrap_or_else(|_| {
            "<h1 style='font-family:sans-serif;text-align:center;margin-top:4rem'>404 — Page introuvable</h1>".to_string()
        });
    (StatusCode::NOT_FOUND, Html(html))
}

/// Fallback handler — redirects any undefined route to /404-not-found.
pub async fn handler_404() -> impl IntoResponse {
    axum::response::Redirect::to("/404-not-found")
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
