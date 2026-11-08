/// Authentication handlers — login, logout, and first-login password setup.
///
/// Session management is cookie-based: a plain `user_id=<id>` HTTP-only cookie.
/// There are no JWT tokens or server-side sessions; the cookie value is trusted as-is
/// (the cookie is not signed, so this is designed for a trusted home-network deployment).
///
/// Password hashing uses Argon2id via the `argon2` crate.  Passwords are never stored
/// in plaintext — only the hash string is kept in `users.password_hash`.
///
/// Two login flows exist:
///   • First login (password_hash IS NULL): user must set a password via the "set" modal.
///   • Returning user (password_hash IS NOT NULL): password is verified against the stored hash.
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::{Deserialize, Serialize};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};

use crate::{
    db::get_setting,
    error::{AppError, AppResult},
    models::{Bank, User},
    AppState,
};

/// Safe subset of User sent to the login template — never exposes password_hash.
#[derive(Serialize)]
struct UserLoginView {
    id: i64,
    name: String,
    last_name: Option<String>,
    /// True when the user already has a password set (determines which modal to show).
    has_password: bool,
}

/// Query parameters for the login page — used to surface error banners.
#[derive(Deserialize, Default)]
pub struct LoginQuery {
    pub error: Option<String>,
    pub unauthorized: Option<String>,
    pub user_error: Option<String>,
}

/// GET /login — show the login page, or redirect to / if already logged in.
pub async fn get_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<LoginQuery>,
) -> AppResult<impl IntoResponse> {
    // If a cookie is present, verify the user still exists in the DB.
    if let Some(cookie) = jar.get("user_id") {
        let uid: Option<i64> = cookie.value().parse().ok();
        let exists = if let Some(id) = uid {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE id = ?")
                .bind(id)
                .fetch_one(&state.db)
                .await
                .unwrap_or(0) > 0
        } else {
            false
        };
        if exists {
            return Ok(Redirect::to("/").into_response());
        }
        // Stale cookie (user was deleted) — clear it and show login page
        let clean_jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
        let html = render_login_page(&state, &query).await?;
        return Ok((clean_jar, html).into_response());
    }

    let html = render_login_page(&state, &query).await?;
    Ok(html.into_response())
}

/// Build and render the login page template with the list of users and banks.
async fn render_login_page(state: &crate::AppState, query: &LoginQuery) -> AppResult<Html<String>> {
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    // Map to the safe view — strip the password hash before passing to the template.
    let user_views: Vec<UserLoginView> = users
        .into_iter()
        .map(|u| UserLoginView {
            id: u.id,
            name: u.name,
            last_name: u.last_name,
            has_password: u.password_hash.is_some(),
        })
        .collect();

    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    let theme = get_setting(&state.db, "theme")
        .await
        .unwrap_or_else(|_| "light".to_string());

    let mut ctx = tera::Context::new();
    ctx.insert("users", &user_views);
    ctx.insert("banks", &banks);
    ctx.insert("theme", &theme);
    ctx.insert("is_dark", &(theme == "dark"));
    ctx.insert("login_error", &query.error.is_some());
    ctx.insert("unauthorized", &query.unauthorized.is_some());
    ctx.insert("user_error", &query.user_error.as_deref().unwrap_or(""));

    Ok(Html(state.tera.render("pages/auth/login.html", &ctx)?))
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub user_id: i64,
    /// Submitted when verifying an existing password.
    pub password: Option<String>,
    /// Submitted when setting a password for the first time.
    pub new_password: Option<String>,
}

/// POST /login — handle both the first-login (set password) and returning-user flows.
pub async fn post_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> AppResult<impl IntoResponse> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(form.user_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;

    if user.password_hash.is_none() {
        // ── First login: user is setting their password for the first time ──────────
        let new_pw = form.new_password.as_deref().unwrap_or("");
        if new_pw.is_empty() {
            return Ok(Redirect::to("/login?error=1").into_response());
        }
        // Generate a random salt and hash the password with Argon2id.
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(new_pw.as_bytes(), &salt)
            .map_err(|e| AppError::Other(e.to_string()))?
            .to_string();
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(&hash)
            .bind(form.user_id)
            .execute(&state.db)
            .await?;
        tracing::info!(user_id = %form.user_id, "Password set on first login");
    } else {
        // ── Returning user: verify their existing password ───────────────────────────
        let submitted = form.password.as_deref().unwrap_or("");
        if submitted.is_empty() {
            return Ok(Redirect::to("/login?error=1").into_response());
        }
        let stored = user.password_hash.as_ref().unwrap();
        let parsed = PasswordHash::new(stored).map_err(|e| AppError::Other(e.to_string()))?;
        if Argon2::default().verify_password(submitted.as_bytes(), &parsed).is_err() {
            tracing::warn!(user_id = %form.user_id, "Failed login attempt");
            return Ok(Redirect::to("/login?error=1").into_response());
        }
    }

    // Record the login timestamp and set the session cookie.
    sqlx::query("UPDATE users SET last_login = datetime('now') WHERE id = ?")
        .bind(form.user_id)
        .execute(&state.db)
        .await?;

    // `http_only` prevents JavaScript from reading the cookie — a small XSS mitigation.
    let cookie = Cookie::build(("user_id", form.user_id.to_string()))
        .path("/")
        .http_only(true)
        .build();
    tracing::info!(user_id = %form.user_id, "User logged in");
    Ok((jar.add(cookie), Redirect::to("/")).into_response())
}

/// POST /logout — clear the session cookie and redirect to login.
pub async fn post_logout(jar: CookieJar) -> impl IntoResponse {
    let jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
    (jar, Redirect::to("/login"))
}
