/// Settings page handlers — app preferences, user management, and password changes.
///
/// App preferences (theme, default currency) are stored in the `settings` key/value table
/// and take effect immediately without a page reload (HTMX saves silently).
///
/// User management allows creating, editing, and deleting users.  Password changes use
/// Argon2id hashing, same as the auth flow.  Each user can edit only their own profile
/// (name, bank, password — changing a password requires the old one); only the admin
/// deletes users.
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::cookie::{Cookie, SignedCookieJar};
use serde::{Deserialize, Serialize};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};

use crate::{clock::UserClock, db::{get_setting, set_setting}, error::{AppError, AppResult}, models::{Bank, Currency, User}, sse::AppEvent, AppState};
use super::{is_htmx, htmx_inline_error, htmx_redirect, toast_response};

/// Query parameters for the settings page — error messages after a redirect (shown as a toast).
#[derive(Deserialize, Default)]
pub struct SettingsQuery {
    pub pw_error: Option<String>,
    pub user_error: Option<String>,
}

/// GET /settings — render the full settings page.
pub async fn get_settings(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    clock: UserClock,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let mut ctx = super::base_context(&jar, &state.db, "/settings").await?;

    let currencies = sqlx::query_as::<_, Currency>("SELECT * FROM currencies ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let users = users_in_local_time(&state, &clock).await?;

    let default_currency_id: i64 = get_setting(&state.db, "default_currency_id")
        .await
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .unwrap_or(1);

    ctx.insert("currencies", &currencies);
    ctx.insert("banks", &banks);
    ctx.insert("users", &users);
    ctx.insert("default_currency_id", &default_currency_id);
    // Messages after a plain (non-HTMX) form post redirected back here.
    let flash = match (query.pw_error.as_deref(), query.user_error.as_deref()) {
        (Some("wrong"), _) => Some("Mot de passe actuel incorrect."),
        (Some("mismatch"), _) => Some("Les nouveaux mots de passe ne correspondent pas."),
        (Some(_), _) => Some("Une erreur est survenue."),
        (_, Some("duplicate")) => Some("Un utilisateur avec ce prénom existe déjà."),
        (_, Some("noname")) => Some("Le prénom est obligatoire."),
        (_, Some("empty")) => Some("Le mot de passe est obligatoire."),
        (_, Some("mismatch")) => Some("Les mots de passe ne correspondent pas."),
        _ => None,
    };
    if let Some(msg) = flash {
        super::insert_flash(&mut ctx, msg, false);
    }

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
/// HTMX callers get a toast; a plain form submit is redirected back to /settings.
pub async fn post_settings(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> AppResult<impl IntoResponse> {
    let actor_id = super::get_user_id(&jar)?;
    // Sanitize theme — only "dark" or "light" are valid values.
    let theme = if form.theme == "dark" { "dark" } else { "light" };
    let currency_id: Option<i64> = match form.default_currency_id.parse::<i64>() {
        Ok(id) => sqlx::query_scalar("SELECT id FROM currencies WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?,
        Err(_) => None,
    };
    let currency_id = currency_id.ok_or_else(|| AppError::Other("Devise inconnue.".into()))?;

    // A new default currency converts every transaction first (exchange rates at each
    // transaction's date). If that fails nothing is saved — the theme neither — and the
    // error shows as a toast (the form keeps the old values).
    // Also when some transactions are still in another currency (from before conversions
    // existed): saving the preferences converts them.
    let unconverted: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM transactions WHERE currency_id != ?)")
        .bind(currency_id)
        .fetch_one(&state.db)
        .await?;
    let mut converted = None;
    if unconverted || currency_id != crate::currency::default_currency_id(&state.db).await {
        match super::transactions::change_default_currency(&state, currency_id).await? {
            Ok(count) => converted = Some(count),
            Err(msg) => return Err(AppError::Other(msg)),
        }
    }
    set_setting(&state.db, "theme", theme).await?;

    state.events.send(AppEvent { kind: "settings", actor_id });
    if converted.is_some() {
        state.events.send(AppEvent { kind: "transactions", actor_id });
    }

    if is_htmx(&headers) {
        let msg = match converted {
            Some(count) => format!("Préférences enregistrées — {} transaction(s) converties dans la nouvelle devise.", count),
            None => "Préférences enregistrées.".to_string(),
        };
        Ok(toast_response(&msg, true, &["transactions-changed"]))
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// `bank_id` from a form select: "" ("— Aucune —") or anything unparsable means no bank.
fn parse_bank_id(value: &Option<String>) -> Option<i64> {
    value.as_deref().and_then(|s| s.parse().ok())
}

/// Form for the "add user" section at the bottom of the users card.
#[derive(Deserialize)]
pub struct NewUserForm {
    pub name: String,
    pub last_name: Option<String>,
    /// A string because "— Aucune —" submits "" (see `parse_bank_id`).
    pub bank_id: Option<String>,
    /// Required.
    pub password: Option<String>,
    pub password_confirm: Option<String>,
    /// If set to "/login", redirect back there after creation (used from the first-run modal).
    pub redirect_to: Option<String>,
}

/// POST /settings/users — create a new user.
///
/// Allowed while logged in, or on first run (no users yet) from the login page.
/// From the settings page (HTMX): answers with a toast, `users-changed` (the users table
/// re-fetches itself) and `user-created` (the form resets). From the login page (plain
/// form post): redirects.
pub async fn post_new_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<NewUserForm>,
) -> AppResult<impl IntoResponse> {
    let htmx = is_htmx(&headers);

    // Only the first-run form (no user exists yet) may create a user without being
    // logged in; after that, new users are added from the settings page.
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await?;
    if user_count > 0 {
        super::get_user_id(&jar)?;
    }
    let name = form.name.trim();
    if name.is_empty() {
        if htmx {
            return Ok(toast_response("Le prénom est obligatoire.", false, &[]));
        }
        return Ok(Redirect::to(match form.redirect_to.as_deref() {
            Some("/login") => "/login?user_error=noname",
            _ => "/settings?user_error=noname",
        }).into_response());
    }
    // Case-insensitive duplicate check on first name.
    let duplicate: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE LOWER(name) = LOWER(?)"
    )
    .bind(name)
    .fetch_optional(&state.db)
    .await?;

    if duplicate.is_some() {
        if htmx {
            return Ok(toast_response("Un utilisateur avec ce prénom existe déjà.", false, &[]));
        }
        let redirect = match form.redirect_to.as_deref() {
            Some("/login") => "/login?user_error=duplicate",
            _ => "/settings?user_error=duplicate",
        };
        return Ok(Redirect::to(redirect).into_response());
    }

    // A password is required, and must match its confirmation.
    let password = form.password.as_deref().unwrap_or("");
    let password_error = if password.is_empty() {
        Some(("empty", "Le mot de passe est obligatoire."))
    } else if form.password_confirm.as_deref().unwrap_or("") != password {
        Some(("mismatch", "Les mots de passe ne correspondent pas."))
    } else {
        None
    };
    if let Some((code, msg)) = password_error {
        if htmx {
            return Ok(toast_response(msg, false, &[]));
        }
        let redirect = match form.redirect_to.as_deref() {
            Some("/login") => format!("/login?user_error={}", code),
            _ => format!("/settings?user_error={}", code),
        };
        return Ok(Redirect::to(&redirect).into_response());
    }
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AppError::Other(e.to_string()))?
        .to_string();

    // The first user — or the next one after the admin deleted their own account —
    // becomes the admin.
    let has_admin: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE is_admin = 1)")
        .fetch_one(&state.db)
        .await?;
    let empty_last_name = form.last_name.as_deref().filter(|s| !s.is_empty());
    let result = sqlx::query(
        "INSERT INTO users (name, last_name, bank_id, password_hash, is_admin) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(name)
    .bind(empty_last_name)
    .bind(parse_bank_id(&form.bank_id))
    .bind(&password_hash)
    .bind(!has_admin)
    .execute(&state.db)
    .await?;
    if !has_admin {
        tracing::info!(user_id = %result.last_insert_rowid(), "New user is the admin");
    }

    state.events.send(AppEvent { kind: "users", actor_id: result.last_insert_rowid() });

    if htmx {
        return Ok(toast_response("Utilisateur créé.", true, &["users-changed", "user-created"]));
    }
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
    /// A string because "— Aucune —" submits "" (see `parse_bank_id`).
    pub bank_id: Option<String>,
    /// Only required when changing an existing password.
    pub old_password: Option<String>,
    pub new_password: Option<String>,
    pub new_password_confirm: Option<String>,
}

/// POST /settings/users/:id/edit — update a user's profile and optionally their password.
/// When called from HTMX, returns the refreshed user row HTML for in-place swap.
pub async fn post_edit_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<EditUserForm>,
) -> AppResult<impl IntoResponse> {
    let htmx = is_htmx(&headers);
    if super::get_user_id(&jar)? != id {
        return Err(AppError::Other("Vous ne pouvez modifier que votre propre profil.".into()));
    }
    // Treat empty string last_name as None (empty <input> submits "").
    let last_name = form.last_name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let name = form.name.trim();
    let name_error = if name.is_empty() {
        Some("Le prénom est obligatoire.")
    } else {
        let duplicate: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE LOWER(name) = LOWER(?) AND id != ?)",
        )
        .bind(name)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
        duplicate.then_some("Un utilisateur avec ce prénom existe déjà.")
    };
    if let Some(msg) = name_error {
        return if htmx {
            Ok(htmx_inline_error("#edit-user-error", msg))
        } else {
            Ok(Redirect::to("/settings?user_error=duplicate").into_response())
        };
    }

    // Password change is optional — only run this block if a new password was supplied.
    // All checks happen before anything is written, so a rejected password change
    // leaves the profile untouched too.
    let mut new_hash: Option<String> = None;
    if let Some(new_pw) = form.new_password.as_deref().filter(|s| !s.is_empty()) {
        let confirm = form.new_password_confirm.as_deref().unwrap_or("");
        if new_pw != confirm {
            return if htmx {
                Ok(htmx_inline_error("#edit-user-error", "Les nouveaux mots de passe ne correspondent pas."))
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
                    Ok(htmx_inline_error("#edit-user-error", "Mot de passe actuel incorrect."))
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
    .bind(name)
    .bind(last_name)
    .bind(parse_bank_id(&form.bank_id))
    .bind(&new_hash)
    .bind(id)
    .execute(&state.db)
    .await?;
    if new_hash.is_some() {
        tracing::info!(user_id = %id, "Password updated via edit modal");
    }

    tracing::info!(user_id = %id, "User profile updated");
    state.events.send(AppEvent { kind: "users", actor_id: id });

    if htmx {
        // The whole users table re-fetches itself (`users-changed`); the modal closes.
        Ok(toast_response("Profil enregistré.", true, &["close-modal", "users-changed"]))
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// Render a single user row for the settings table (used after edit to do an in-place swap).
/// User fields the edit modal is pre-filled with (never exposes the password hash).
#[derive(Serialize)]
struct EditUserView {
    id: i64,
    name: String,
    last_name: String,
    bank_id: String,
    has_password: bool,
}

/// GET /settings/users/:id/delete — the delete confirmation modal (admin only, like the
/// deletion itself).
pub async fn get_delete_user_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let viewer = super::get_current_user(&jar, &state.db).await?;
    if !viewer.is_admin {
        return Err(AppError::Other("Seul l'administrateur peut supprimer des utilisateurs.".into()));
    }
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;
    let mut ctx = tera::Context::new();
    ctx.insert("user", &serde_json::json!({ "id": u.id, "name": u.name, "last_name": u.last_name }));
    ctx.insert("is_self", &(u.id == viewer.id));
    Ok(Html(state.tera.render("modal/user_delete.html", &ctx)?))
}

/// GET /settings/users/:id/edit — the edit-user modal (loaded into `#modal-root`); only
/// for the logged-in user's own profile.
pub async fn get_edit_user_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    if super::get_user_id(&jar)? != id {
        return Err(AppError::Other("Vous ne pouvez modifier que votre propre profil.".into()));
    }
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let user = EditUserView {
        id: u.id,
        name: u.name,
        last_name: u.last_name.unwrap_or_default(),
        bank_id: u.bank_id.map(|b| b.to_string()).unwrap_or_default(),
        has_password: u.password_hash.is_some(),
    };
    let mut ctx = tera::Context::new();
    ctx.insert("user", &user);
    ctx.insert("banks", &banks);
    Ok(Html(state.tera.render("modal/user_edit.html", &ctx)?))
}

/// GET /settings/users — the users table fragment (`_users_table.html`), re-fetched
/// whole after any user is added, edited or deleted.
pub async fn get_users_table(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    clock: UserClock,
) -> AppResult<Html<String>> {
    let viewer = super::get_current_user(&jar, &state.db).await?;
    let users = users_in_local_time(&state, &clock).await?;
    let banks = sqlx::query_as::<_, Bank>("SELECT * FROM banks ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    let mut ctx = tera::Context::new();
    ctx.insert("users", &users);
    ctx.insert("banks", &banks);
    ctx.insert("current_user", &viewer);
    Ok(Html(state.tera.render("pages/settings/_users_table.html", &ctx)?))
}

/// DELETE /settings/users/:id — delete a user.
/// Automatically logs out if the user deletes their own account, or if no users remain.
pub async fn delete_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    let current_user_id = super::get_user_id(&jar).ok();
    let is_self = current_user_id.map_or(false, |uid| uid == id);

    // Only the admin can delete users (others, or their own account).
    let is_admin: bool = match current_user_id {
        Some(uid) => sqlx::query_scalar("SELECT is_admin FROM users WHERE id = ?")
            .bind(uid)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(false),
        None => false,
    };
    if !is_admin {
        if is_htmx(&headers) {
            return Ok(toast_response("Seul l'administrateur peut supprimer des utilisateurs.", false, &[]));
        }
        return Err(AppError::Unauthorized);
    }

    // Foreign keys refuse to delete a user who still owns transactions, shares or files.
    if let Err(e) = sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(&state.db).await {
        tracing::warn!(user_id = %id, err = %e, "User delete refused");
        if is_htmx(&headers) {
            return Ok(toast_response(
                "Impossible de supprimer cet utilisateur : il a encore des transactions, des parts ou des fichiers importés.",
                false,
                &["users-changed"],
            ));
        }
        return Err(e.into());
    }

    tracing::info!(user_id = %id, self_delete = %is_self, "User deleted");
    state.events.send(AppEvent { kind: "users", actor_id: current_user_id.unwrap_or(0) });

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
        // The whole users table re-fetches itself (`users-changed`).
        Ok(toast_response("Utilisateur supprimé.", true, &["users-changed"]))
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// Every user, with `last_login` (stored in UTC) in the viewer's local time.
async fn users_in_local_time(state: &AppState, clock: &UserClock) -> AppResult<Vec<User>> {
    let mut users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;
    for u in &mut users {
        u.last_login = u.last_login.as_deref().map(|t| clock.local(t));
    }
    Ok(users)
}
