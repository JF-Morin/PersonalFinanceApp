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
use axum_extra::extract::cookie::SignedCookieJar;

/// Extract the logged-in user's ID from the `user_id` session cookie.
/// Returns `Err(AppError::Unauthorized)` if the cookie is absent or not a valid integer,
/// which Axum converts to a redirect to /login via `AppError::into_response`.
pub fn get_user_id(jar: &SignedCookieJar) -> Result<i64, AppError> {
    jar.get("user_id")
        .and_then(|c| c.value().parse::<i64>().ok())
        .ok_or(AppError::Unauthorized)
}

/// Full user lookup — same as `get_user_id` but also fetches the User row from the DB.
/// Used when the template needs the user's name, bank, etc. (most full-page renders).
pub async fn get_current_user(jar: &SignedCookieJar, db: &DbPool) -> Result<User, AppError> {
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

/// `HX-Trigger` header that shows the global toast (`layout/base.html`) and fires each of
/// `events` on the page (e.g. `files-changed` makes a table re-fetch itself,
/// `close-modal` closes the open server-rendered modal).
pub fn toast_header(msg: &str, ok: bool, events: &[&str]) -> (axum::http::HeaderName, String) {
    let mut trigger = serde_json::json!({ "toast": { "msg": msg, "ok": ok } });
    for event in events {
        trigger[*event] = serde_json::Value::Bool(true);
    }
    hx_trigger(trigger)
}

/// `HX-Trigger` header that only fires `events` (no toast).
pub fn events_header(events: &[&str]) -> (axum::http::HeaderName, String) {
    let mut trigger = serde_json::json!({});
    for event in events {
        trigger[*event] = serde_json::Value::Bool(true);
    }
    hx_trigger(trigger)
}

fn hx_trigger(trigger: serde_json::Value) -> (axum::http::HeaderName, String) {
    // Header values reach the browser as Latin-1, so escape non-ASCII ("é", "—") as \uXXXX.
    let mut header = String::new();
    for c in trigger.to_string().chars() {
        if c.is_ascii() {
            header.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                header.push_str(&format!("\\u{:04x}", unit));
            }
        }
    }
    (axum::http::header::HeaderName::from_static("hx-trigger"), header)
}

/// Show the global toast once when the page loads — for messages after a plain
/// form-post redirect (e.g. `/login?error=1`), where there's no HTMX response to carry
/// an HX-Trigger. Read by the toast's `x-init` in `layout/shell.html`.
pub fn insert_flash(ctx: &mut tera::Context, msg: &str, ok: bool) {
    ctx.insert("flash", &serde_json::json!({ "msg": msg, "ok": ok }));
}

/// Empty 200 response carrying [`toast_header`].
pub fn toast_response(msg: &str, ok: bool, events: &[&str]) -> axum::response::Response {
    (StatusCode::OK, [toast_header(msg, ok, events)], "").into_response()
}

/// 200 response that HTMX swaps into `target` instead of the request's own target —
/// used to show a validation message inside a modal without closing it.
pub fn htmx_inline_error(target: &str, msg: &str) -> axum::response::Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::HeaderName::from_static("hx-retarget"), target.to_string()),
            (axum::http::header::HeaderName::from_static("hx-reswap"), "innerHTML".to_string()),
        ],
        msg.to_string(),
    )
        .into_response()
}

/// Middleware: a redirect sent to an HTMX request becomes `HX-Redirect`. The browser would
/// otherwise follow a 3xx silently and HTMX would swap the target page into the element
/// that made the request — e.g. the whole login page into a modal once a session is no
/// longer valid (`AppError::Unauthorized` redirects to /login). Cookies set on the
/// response are kept.
pub async fn htmx_redirects(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let htmx = is_htmx(req.headers());
    let mut res = next.run(req).await;
    if htmx && res.status().is_redirection() {
        if let Some(location) = res.headers_mut().remove(axum::http::header::LOCATION) {
            *res.status_mut() = StatusCode::OK;
            res.headers_mut().insert(axum::http::header::HeaderName::from_static("hx-redirect"), location);
        }
    }
    res
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

/// Build a Tera context pre-populated with the current user, UI theme and the active
/// nav section (`active_nav`, the section's URL — the sidebar and bottom nav highlight it).
/// Every full-page handler calls this first so the base layout always has what it needs.
pub async fn base_context(jar: &SignedCookieJar, db: &DbPool, active_nav: &str) -> Result<tera::Context, AppError> {
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
    ctx.insert("active_nav", active_nav);
    Ok(ctx)
}
