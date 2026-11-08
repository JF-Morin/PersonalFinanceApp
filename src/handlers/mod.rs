/// Shared utilities used by all handlers.
///
/// Every page handler follows the same three-step pattern:
///   1. Call `get_user_id` or `get_current_user` to authenticate — returns
///      `Err(AppError::Unauthorized)` and redirects to /login if the cookie is absent.
///   2. Call `base_context` to get a Tera context pre-populated with `current_user` and `theme`.
///   3. Add page-specific data to the context, then call `state.tera.render(...)`.
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

/// Extract the logged-in user's ID from the `user_id` session cookie.
/// Returns `Err(AppError::Unauthorized)` if the cookie is absent or not a valid integer,
/// which Axum converts to a redirect to /login via `AppError::into_response`.
pub fn get_user_id(jar: &CookieJar) -> Result<i64, AppError> {
    jar.get("user_id")
        .and_then(|c| c.value().parse::<i64>().ok())
        .ok_or(AppError::Unauthorized)
}

/// Full user lookup — same as `get_user_id` but also fetches the User row from the DB.
/// Used when the template needs the user's name, bank, etc. (most full-page renders).
pub async fn get_current_user(jar: &CookieJar, db: &DbPool) -> Result<User, AppError> {
    let user_id = get_user_id(jar)?;
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::Unauthorized)
}

/// True when the request was made by HTMX (carries `HX-Request: true` header).
/// Handlers use this to decide between returning a full HTML page or a partial fragment.
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

/// Return an HTMX client-side redirect via the `HX-Redirect` response header.
/// A normal HTTP 3xx redirect is ignored by HTMX (it fetches the redirect target
/// as a partial), so we use this header instead to tell HTMX to navigate the full page.
pub fn htmx_redirect(url: &str) -> impl IntoResponse {
    ([(axum::http::header::HeaderName::from_static("hx-redirect"), url.to_string())], "")
}

/// Dedicated 404 page at /404-not-found.
pub async fn get_not_found(State(state): State<AppState>) -> impl IntoResponse {
    let html = state
        .tera
        .render("errors/404.html", &tera::Context::new())
        .unwrap_or_else(|_| {
            "<h1 style='font-family:sans-serif;text-align:center;margin-top:4rem'>404 — Page introuvable</h1>".to_string()
        });
    (StatusCode::NOT_FOUND, Html(html))
}

/// Fallback handler — redirects any undefined route to /404-not-found.
pub async fn handler_404() -> impl IntoResponse {
    axum::response::Redirect::to("/404-not-found")
}

/// Build a Tera context pre-populated with the current user and UI theme.
/// Every full-page handler calls this first so the base layout always has what it needs.
pub async fn base_context(jar: &CookieJar, db: &DbPool) -> Result<tera::Context, AppError> {
    let user = get_current_user(jar, db).await?;
    let theme = crate::db::get_setting(db, "theme")
        .await
        .unwrap_or_else(|_| "light".to_string());

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &user);
    ctx.insert("theme", &theme);
    // `is_dark` is a convenience bool so templates can write `{% if is_dark %}` instead of
    // `{% if theme == "dark" %}`.
    ctx.insert("is_dark", &(theme == "dark"));
    Ok(ctx)
}
