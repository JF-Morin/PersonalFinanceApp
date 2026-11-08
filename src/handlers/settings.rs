/// Settings page handlers — app preferences, user management, and password changes.
///
/// App preferences (theme, default currency) are stored in the `settings` key/value table
/// and take effect immediately without a page reload (HTMX saves silently).
///
/// User management allows creating, editing, and deleting users.  Password changes use
/// Argon2id hashing, same as the auth flow.  Editing another user's profile (name/bank)
/// is allowed by any logged-in user, but changing a password requires the old password.
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};

use crate::{db::{get_setting, set_setting}, error::{AppError, AppResult}, models::{Bank, Currency, User}, sse::AppEvent, AppState};
use super::{is_htmx, htmx_redirect};

/// Query parameters for the settings page — used to surface feedback banners.
#[derive(Deserialize, Default)]
pub struct SettingsQuery {
    pub pw_error: Option<String>,
    pub user_error: Option<String>,
}

/// GET /settings — render the full settings page.
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
    ctx.insert("pw_error", &query.pw_error.as_deref().unwrap_or(""));
    ctx.insert("user_error", &query.user_error.as_deref().unwrap_or(""));

    let html = state.tera.render("pages/settings/settings.html", &ctx)?;
    Ok(Html(html))
}

/// Form for the app preferences section (theme + default currency).
#[derive(Deserialize)]
pub struct SettingsForm {
    pub theme: String,
    pub default_currency_id: String,
}

/// POST /settings — save app preferences and broadcast a "settings" SSE event.
/// Returns empty HTML for HTMX (silent save) or a redirect for plain form submit.
pub async fn post_settings(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> AppResult<impl IntoResponse> {
    let actor_id = super::get_user_id(&jar)?;
    // Sanitize theme — only "dark" or "light" are valid values.
    let theme = if form.theme == "dark" { "dark" } else { "light" };
    set_setting(&state.db, "theme", theme).await?;
    set_setting(&state.db, "default_currency_id", &form.default_currency_id).await?;

    state.events.send(AppEvent { kind: "settings", actor_id });

    if is_htmx(&headers) {
        Ok("".into_response())
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// Form for the "add user" section at the bottom of the users card.
#[derive(Deserialize)]
pub struct NewUserForm {
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
    pub password: Option<String>,
    /// If set to "/login", redirect back there after creation (used from the first-run modal).
    pub redirect_to: Option<String>,
}

/// POST /settings/users — create a new user.
pub async fn post_new_user(
    State(state): State<AppState>,
    _jar: CookieJar,
    Form(form): Form<NewUserForm>,
) -> AppResult<impl IntoResponse> {
    // Case-insensitive duplicate check on first name.
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

    // Hash the password immediately if provided; otherwise leave it None
    // (user will set a password on their first login).
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

    let result = sqlx::query("INSERT INTO users (name, last_name, bank_id, password_hash) VALUES (?, ?, ?, ?)")
        .bind(&form.name)
        .bind(&form.last_name)
        .bind(form.bank_id)
        .bind(&password_hash)
        .execute(&state.db)
        .await?;

    state.events.send(AppEvent { kind: "settings", actor_id: result.last_insert_rowid() });

    let redirect = match form.redirect_to.as_deref() {
        Some("/login") => "/login",
        _ => "/settings",
    };
    Ok(Redirect::to(redirect).into_response())
}

/// Form for the user edit modal (name, bank, optional password change).
#[derive(Deserialize)]
pub struct EditUserForm {
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
    /// Only required when changing an existing password.
    pub old_password: Option<String>,
    pub new_password: Option<String>,
    pub new_password_confirm: Option<String>,
}

/// POST /settings/users/:id/edit — update a user's profile and optionally their password.
/// When called from HTMX, returns the refreshed user row HTML for in-place swap.
pub async fn post_edit_user(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<EditUserForm>,
) -> AppResult<impl IntoResponse> {
    let htmx = is_htmx(&headers);
    super::get_user_id(&jar)?;
    // Treat empty string last_name as None (empty <input> submits "").
    let last_name = form.last_name.as_deref().filter(|s| !s.is_empty());

    // Password change is optional — only run this block if a new password was supplied.
    // All checks happen before anything is written, so a rejected password change
    // leaves the profile untouched too.
    let mut new_hash: Option<String> = None;
    if let Some(new_pw) = form.new_password.as_deref().filter(|s| !s.is_empty()) {
        let confirm = form.new_password_confirm.as_deref().unwrap_or("");
        if new_pw != confirm {
            return if htmx {
                Ok((StatusCode::UNPROCESSABLE_ENTITY, "Les nouveaux mots de passe ne correspondent pas.").into_response())
            } else {
                Ok(Redirect::to("/settings?pw_error=mismatch").into_response())
            };
        }

        // Verify old password only if the user already has one set.
        // A user with no password hash can set one freely (first-time setup).
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
                return if htmx {
                    Ok((StatusCode::UNPROCESSABLE_ENTITY, "Mot de passe actuel incorrect.").into_response())
                } else {
                    Ok(Redirect::to("/settings?pw_error=wrong").into_response())
                };
            }
        }

        let salt = SaltString::generate(&mut OsRng);
        new_hash = Some(
            Argon2::default()
                .hash_password(new_pw.as_bytes(), &salt)
                .map_err(|e| crate::error::AppError::Other(e.to_string()))?
                .to_string(),
        );
    }

    // Single statement so the profile and the password are saved together.
    // COALESCE keeps the existing hash when no new password was supplied.
    sqlx::query(
        "UPDATE users SET name = ?, last_name = ?, bank_id = ?, \
         password_hash = COALESCE(?, password_hash) WHERE id = ?"
    )
    .bind(&form.name)
    .bind(last_name)
    .bind(form.bank_id)
    .bind(&new_hash)
    .bind(id)
    .execute(&state.db)
    .await?;
    if new_hash.is_some() {
        tracing::info!(user_id = %id, "Password updated via edit modal");
    }

    tracing::info!(user_id = %id, "User profile updated");
    state.events.send(AppEvent { kind: "settings", actor_id: id });

    if htmx {
        // Return the refreshed row HTML so the HTMX hx-target swap updates the table in place.
        let html = render_user_row(&state, id).await?;
        Ok(Html(html).into_response())
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// Render a single user row for the settings table (used after edit to do an in-place swap).
async fn render_user_row(state: &AppState, user_id: i64) -> AppResult<String> {
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let mut ctx = tera::Context::new();
    ctx.insert("u", &u);
    ctx.insert("banks", &banks);
    Ok(state.tera.render("pages/settings/_user_row.html", &ctx)?)
}

/// DELETE /settings/users/:id — delete a user.
/// Automatically logs out if the user deletes their own account, or if no users remain.
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
    state.events.send(AppEvent { kind: "settings", actor_id: current_user_id.unwrap_or(0) });

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await?;

    // Log out when the user deletes themselves, or when no users remain.
    if is_self || remaining == 0 {
        let new_jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
        return if is_htmx(&headers) {
            // Use HX-Redirect instead of a normal 3xx so HTMX navigates the full page.
            Ok((new_jar, htmx_redirect("/login")).into_response())
        } else {
            Ok((new_jar, Redirect::to("/login")).into_response())
        };
    }

    if is_htmx(&headers) {
        // Return empty string — HTMX `hx-swap="outerHTML"` removes the row element.
        Ok("".into_response())
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}
