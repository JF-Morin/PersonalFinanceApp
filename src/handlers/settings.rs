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
    // Admin only: log level and retention.
    let (log_level, log_keep_days) = crate::logs::saved(&state.db).await;
    ctx.insert("log_levels", &crate::logs::LEVELS);
    ctx.insert("log_level", &log_level);
    ctx.insert("log_keep_days", &log_keep_days);
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

/// Form of the admin's "Journaux" card.
#[derive(Deserialize)]
pub struct LogSettingsForm {
    /// One of `logs::LEVELS`, or "" for the default (`RUST_LOG`).
    pub log_level: String,
    pub log_keep_days: String,
}

/// POST /settings/logs — the log level and how many days of logs to keep (admin only).
/// Both apply at once: the new level without a restart, older log files deleted now.
pub async fn post_log_settings(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<LogSettingsForm>,
) -> AppResult<axum::response::Response> {
    let viewer = require_admin(&jar, &state.db).await?;
    let level = form.log_level.trim();
    if !level.is_empty() && !crate::logs::LEVELS.contains(&level) {
        return Err(AppError::Other("Niveau de journalisation inconnu.".into()));
    }
    let keep_days: u32 = match form.log_keep_days.trim().parse() {
        Ok(n) if n <= 3650 => n,
        _ => return Err(AppError::Other("Durée de conservation invalide (0 à 3650 jours).".into())),
    };

    let mut tx = state.db.begin().await?;
    set_setting(&mut *tx, "log_level", level).await?;
    set_setting(&mut *tx, "log_keep_days", &keep_days.to_string()).await?;
    tx.commit().await?;
    state.logs.set_level(level).map_err(|e| AppError::Other(e.to_string()))?;
    let deleted = state.logs.prune(keep_days);
    tracing::info!(by = %viewer.id, level = %level, keep_days, "Log settings changed");

    let msg = if deleted > 0 {
        format!("Journaux enregistrés — {} ancien(s) fichier(s) supprimé(s).", deleted)
    } else {
        "Journaux enregistrés.".to_string()
    };
    Ok(toast_response(&msg, true, &[]))
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

/// What a user still has in the app: owned transactions, shares in transactions, imported
/// files. A user with any of these is deactivated rather than deleted.
#[derive(Serialize, Default)]
struct UserData {
    transactions: i64,
    shares: i64,
    files: i64,
}

impl UserData {
    async fn of(db: &crate::db::DbPool, user_id: i64) -> AppResult<UserData> {
        let (transactions, shares, files): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM transactions WHERE owner_id = ?1), \
                    (SELECT COUNT(*) FROM transaction_shares WHERE user_id = ?1), \
                    (SELECT COUNT(*) FROM imported_files WHERE importer_id = ?1)",
        )
        .bind(user_id)
        .fetch_one(db)
        .await?;
        Ok(UserData { transactions, shares, files })
    }

    fn is_empty(&self) -> bool {
        self.transactions == 0 && self.shares == 0 && self.files == 0
    }
}

/// The user who becomes admin when `leaving_id` (the admin) leaves: the oldest other
/// active user.
async fn next_admin<'e, E: sqlx::SqliteExecutor<'e>>(executor: E, leaving_id: i64) -> AppResult<Option<(i64, String)>> {
    Ok(sqlx::query_as("SELECT id, name FROM users WHERE is_active = 1 AND id != ? ORDER BY id LIMIT 1")
        .bind(leaving_id)
        .fetch_optional(executor)
        .await?)
}

/// Pass the admin role from `leaving_id` to the next admin, if there is one (on the
/// caller's DB transaction). Without one, the next user created becomes admin.
async fn hand_over_admin(conn: &mut sqlx::SqliteConnection, leaving_id: i64) -> AppResult<()> {
    sqlx::query("UPDATE users SET is_admin = 0 WHERE id = ?").bind(leaving_id).execute(&mut *conn).await?;
    if let Some((next_id, _)) = next_admin(&mut *conn, leaving_id).await? {
        sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?").bind(next_id).execute(&mut *conn).await?;
        tracing::info!(user_id = %next_id, "Admin role handed over");
    }
    Ok(())
}

/// The admin making the request, or the reason they can't.
async fn require_admin(jar: &SignedCookieJar, db: &crate::db::DbPool) -> AppResult<User> {
    let viewer = super::get_current_user(jar, db).await?;
    if !viewer.is_admin {
        return Err(AppError::Other("Réservé à l'administrateur.".into()));
    }
    Ok(viewer)
}

/// GET /settings/users/:id/delete — the admin's modal for removing a user: delete when the
/// user has no data, otherwise deactivate (or reactivate, if already deactivated).
pub async fn get_delete_user_modal(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let viewer = require_admin(&jar, &state.db).await?;
    let u = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::Other("Utilisateur introuvable".into()))?;
    let data = UserData::of(&state.db, id).await?;
    let mut ctx = tera::Context::new();
    ctx.insert("user", &serde_json::json!({ "id": u.id, "name": u.name, "last_name": u.last_name, "is_active": u.is_active }));
    ctx.insert("is_self", &(u.id == viewer.id));
    ctx.insert("has_data", &!data.is_empty());
    ctx.insert("data", &data);
    ctx.insert("next_admin", &next_admin(&state.db, u.id).await?.map(|(_, name)| name));
    Ok(Html(state.tera.render("modal/user_delete.html", &ctx)?))
}

/// POST /settings/users/:id/deactivate — deactivate a user (admin only): they can't log
/// in any more (an open session ends on its next request) and aren't offered as a
/// participant; their transactions, shares and files stay. The admin deactivating
/// themselves hands the role to the oldest other active user, and is logged out.
pub async fn post_deactivate_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<axum::response::Response> {
    let viewer = require_admin(&jar, &state.db).await?;
    let is_self = viewer.id == id;
    if is_self && next_admin(&state.db, id).await?.is_none() {
        return Err(AppError::Other("Vous êtes le dernier utilisateur actif : impossible de vous désactiver.".into()));
    }
    let mut tx = state.db.begin().await?;
    let done = sqlx::query("UPDATE users SET is_active = 0 WHERE id = ? AND is_active = 1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if done == 0 {
        return Err(AppError::Other("Utilisateur introuvable ou déjà désactivé.".into()));
    }
    if is_self {
        hand_over_admin(&mut tx, id).await?;
    }
    tx.commit().await?;
    tracing::info!(user_id = %id, by = %viewer.id, "User deactivated");
    state.events.send(AppEvent { kind: "users", actor_id: viewer.id });

    if is_self {
        let jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
        return Ok((jar, htmx_redirect("/login")).into_response());
    }
    Ok(toast_response("Utilisateur désactivé.", true, &["close-modal", "users-changed"]))
}

/// POST /settings/users/:id/reactivate — let a deactivated user log in again (admin only).
pub async fn post_reactivate_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> AppResult<axum::response::Response> {
    let viewer = require_admin(&jar, &state.db).await?;
    sqlx::query("UPDATE users SET is_active = 1 WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    tracing::info!(user_id = %id, by = %viewer.id, "User reactivated");
    state.events.send(AppEvent { kind: "users", actor_id: viewer.id });
    Ok(toast_response("Utilisateur réactivé.", true, &["close-modal", "users-changed"]))
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

    // Only a user without data is deleted; the others are deactivated instead (the modal
    // offers that). The foreign keys would refuse anyway.
    if !UserData::of(&state.db, id).await?.is_empty() {
        let msg = "Cet utilisateur a des transactions, des parts ou des fichiers importés : désactivez-le plutôt.";
        if is_htmx(&headers) {
            return Ok(toast_response(msg, false, &["users-changed"]));
        }
        return Err(AppError::Other(msg.into()));
    }
    let mut tx = state.db.begin().await?;
    if is_self {
        // The admin deleting their own account hands the role over.
        hand_over_admin(&mut tx, id).await?;
    }
    sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;

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
        Ok(toast_response("Utilisateur supprimé.", true, &["close-modal", "users-changed"]))
    } else {
        Ok(Redirect::to("/settings").into_response())
    }
}

/// Every user, with `last_login` (stored in UTC) in the viewer's local time.
async fn users_in_local_time(state: &AppState, clock: &UserClock) -> AppResult<Vec<User>> {
    // Active users first.
    let mut users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY is_active DESC, name")
        .fetch_all(&state.db)
        .await?;
    for u in &mut users {
        u.last_login = u.last_login.as_deref().map(|t| clock.local(t));
    }
    Ok(users)
}

// ── Reset (danger zone) ───────────────────────────────────────────────────────

/// The phrase the admin types in the reset modal to confirm.
const RESET_PHRASE: &str = "TOUT SUPPRIMER";

/// What a reset deletes (shown in the modal before, and in the log after).
#[derive(Serialize, Debug, PartialEq)]
struct ResetCounts {
    users: i64,
    transactions: i64,
    files: i64,
}

async fn reset_counts(db: &crate::db::DbPool) -> AppResult<ResetCounts> {
    let (users, transactions, files): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM users), (SELECT COUNT(*) FROM transactions), (SELECT COUNT(*) FROM imported_files)",
    )
    .fetch_one(db)
    .await?;
    Ok(ResetCounts { users, transactions, files })
}

/// GET /settings/reset — the reset confirmation modal (admin only).
pub async fn get_reset_modal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Html<String>> {
    require_admin(&jar, &state.db).await?;
    let mut ctx = tera::Context::new();
    ctx.insert("counts", &reset_counts(&state.db).await?);
    ctx.insert("phrase", RESET_PHRASE);
    Ok(Html(state.tera.render("modal/reset.html", &ctx)?))
}

/// What the danger-zone modals submit: the typed phrase and the admin's password.
#[derive(Deserialize)]
pub struct ResetForm {
    pub phrase: String,
    pub password: String,
}

/// The danger-zone confirmation: the exact phrase and the admin's password. `Some` is
/// the refusal, shown in the modal (`error_target`).
fn check_confirmation(admin: &User, form: &ResetForm, phrase: &str, error_target: &str) -> Option<axum::response::Response> {
    if form.phrase.trim() != phrase {
        return Some(htmx_inline_error(error_target, &format!("Tapez exactement « {} ».", phrase)));
    }
    let password_ok = admin.password_hash.as_deref().is_some_and(|stored| {
        PasswordHash::new(stored)
            .map(|parsed| Argon2::default().verify_password(form.password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false)
    });
    if !password_ok {
        tracing::warn!(user_id = %admin.id, "Danger-zone action refused: wrong password");
        return Some(htmx_inline_error(error_target, "Mot de passe incorrect."));
    }
    None
}

/// "12,3 Mo", "850 Ko".
fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{:.1} Mo", mb).replace('.', ",")
    } else {
        format!("{} Ko", (bytes as f64 / 1024.0).ceil() as u64)
    }
}

/// The files a danger-zone "delete files" modal is about.
#[derive(Clone, Copy)]
enum DangerFiles {
    Backups,
    Logs,
}

impl DangerFiles {
    fn phrase(self) -> &'static str {
        match self {
            DangerFiles::Backups => "SUPPRIMER LES SAUVEGARDES",
            DangerFiles::Logs => "SUPPRIMER LES JOURNAUX",
        }
    }

    fn files(self, state: &AppState) -> Vec<(std::path::PathBuf, u64)> {
        match self {
            DangerFiles::Backups => crate::backup::backup_files(std::path::Path::new(&state.config.backup_dir)),
            DangerFiles::Logs => state.logs.files(),
        }
    }

    fn delete(self, state: &AppState) -> (usize, u64) {
        match self {
            DangerFiles::Backups => crate::backup::delete_all(std::path::Path::new(&state.config.backup_dir)),
            DangerFiles::Logs => state.logs.delete_all(),
        }
    }

    /// Modal context: title, icon, action URL, what goes, and a word of warning.
    fn render(self, state: &AppState) -> AppResult<Html<String>> {
        let files = self.files(state);
        let bytes: u64 = files.iter().map(|(_, size)| size).sum();
        let mut ctx = tera::Context::new();
        let (title, icon, url, what, note) = match self {
            DangerFiles::Backups => (
                "Supprimer les sauvegardes ?",
                "fa-box-archive",
                "/settings/backups/delete",
                "sauvegarde(s) de la base de données",
                "Sans sauvegarde, une erreur ou une réinitialisation ne pourra plus être annulée.                  Les prochaines sauvegardes automatiques seront créées normalement.",
            ),
            DangerFiles::Logs => (
                "Supprimer les journaux ?",
                "fa-file-lines",
                "/settings/logs/delete",
                "fichier(s) journal",
                "Le journal du jour est vidé plutôt que supprimé : l'application continue d'y écrire.",
            ),
        };
        ctx.insert("title", title);
        ctx.insert("icon", icon);
        ctx.insert("url", url);
        ctx.insert("what", what);
        ctx.insert("note", note);
        ctx.insert("count", &files.len());
        ctx.insert("size", &human_size(bytes));
        ctx.insert("phrase", self.phrase());
        Ok(Html(state.tera.render("modal/danger_files.html", &ctx)?))
    }

    async fn post(self, state: &AppState, jar: &SignedCookieJar, form: &ResetForm) -> AppResult<axum::response::Response> {
        let admin = require_admin(jar, &state.db).await?;
        if let Some(refused) = check_confirmation(&admin, form, self.phrase(), "#danger-files-error") {
            return Ok(refused);
        }
        let (count, bytes) = self.delete(state);
        let label = match self {
            DangerFiles::Backups => "sauvegarde(s) supprimée(s)",
            DangerFiles::Logs => "fichier(s) journal supprimé(s)",
        };
        tracing::warn!(by = %admin.id, count, bytes, kind = label, "Files deleted from the danger zone");
        Ok(toast_response(
            &format!("{} {} — {} libérés.", count, label, human_size(bytes)),
            true,
            &["close-modal"],
        ))
    }
}

/// GET /settings/backups/delete — confirmation modal for deleting every backup (admin only).
pub async fn get_delete_backups_modal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Html<String>> {
    require_admin(&jar, &state.db).await?;
    DangerFiles::Backups.render(&state)
}

/// POST /settings/backups/delete — delete every backup file (admin, phrase + password).
pub async fn post_delete_backups(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<ResetForm>,
) -> AppResult<axum::response::Response> {
    DangerFiles::Backups.post(&state, &jar, &form).await
}

/// GET /settings/logs/delete — confirmation modal for deleting every log file (admin only).
pub async fn get_delete_logs_modal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Html<String>> {
    require_admin(&jar, &state.db).await?;
    DangerFiles::Logs.render(&state)
}

/// POST /settings/logs/delete — delete every log file (admin, phrase + password).
pub async fn post_delete_logs(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<ResetForm>,
) -> AppResult<axum::response::Response> {
    DangerFiles::Logs.post(&state, &jar, &form).await
}

/// POST /settings/reset — erase everything (admin only, after typing `RESET_PHRASE` and
/// their password): every user, transaction, share, imported file (rows and PDFs) and
/// stored exchange rate; preferences back to their defaults. Reference lists (categories,
/// currencies, banks) and the database file stay. A backup is written first. The admin is
/// logged out and the app starts over at the first-run registration; other sessions end on
/// their next request (their user no longer exists).
pub async fn post_reset(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<ResetForm>,
) -> AppResult<axum::response::Response> {
    let admin = require_admin(&jar, &state.db).await?;
    if let Some(refused) = check_confirmation(&admin, &form, RESET_PHRASE, "#reset-error") {
        return Ok(refused);
    }

    let counts = reset_everything(&state).await?;
    tracing::warn!(by = %admin.id, ?counts, "APPLICATION RESET: all data deleted");
    for kind in ["users", "transactions", "imports", "settings"] {
        state.events.send(AppEvent { kind, actor_id: admin.id });
    }
    let jar = jar.remove(Cookie::build(("user_id", "")).path("/").build());
    Ok((jar, htmx_redirect("/login")).into_response())
}

/// The reset itself: backup, then every data table emptied in one DB transaction (ids
/// restart at 1), then — only once committed — the stored PDFs deleted.
async fn reset_everything(state: &AppState) -> AppResult<ResetCounts> {
    let counts = reset_counts(&state.db).await?;
    // The way back if this was a mistake. No backup, no reset.
    let backup = crate::backup::backup(
        &state.db,
        std::path::Path::new(&state.config.backup_dir),
        state.config.backup_keep.max(1),
        "before reset",
    )
    .await
    .map_err(|e| AppError::Other(format!("Sauvegarde impossible, rien n'a été supprimé : {}", e)))?;
    tracing::info!(path = %backup.display(), "Backup written before reset");

    let mut tx = state.db.begin().await?;
    // Children first (foreign keys).
    for table in ["transaction_shares", "transactions", "imported_files", "exchange_rates", "users", "settings"] {
        sqlx::query(&format!("DELETE FROM {table}")).execute(&mut *tx).await?;
    }
    sqlx::query(
        "DELETE FROM sqlite_sequence WHERE name IN ('transaction_shares', 'transactions', 'imported_files', 'users')",
    )
    .execute(&mut *tx)
    .await?;
    // Preferences as on a new install (migration 001); log settings back to their defaults.
    sqlx::query("INSERT INTO settings (key, value) VALUES ('theme', 'light'), ('default_currency_id', '1')")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.uploads.clear();
    // The saved log level is gone: back to the default (RUST_LOG) right away.
    if let Err(e) = state.logs.set_level("") {
        tracing::warn!(error = %e, "Could not reset the log level");
    }

    // The PDFs, now that nothing points to them.
    let statements = std::path::Path::new(&state.config.storage_path).join("Statements");
    if let Ok(entries) = std::fs::read_dir(&statements) {
        for entry in entries.flatten() {
            let path = entry.path();
            let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            if let Err(e) = removed {
                tracing::warn!(path = %path.display(), error = %e, "Could not delete stored statement");
            }
        }
    }
    // Space freed by the deleted rows goes back to the disk.
    if let Err(e) = sqlx::query("VACUUM").execute(&state.db).await {
        tracing::warn!(error = %e, "VACUUM after reset failed");
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::reset_everything;
    use crate::test_support::*;

    /// After a reset: no user, transaction, share, file (row or PDF) or stored rate; default
    /// preferences; ids start at 1 again; reference lists kept; a backup written first.
    #[tokio::test]
    async fn reset_erases_everything_but_the_reference_lists() {
        let app = test_app().await;
        let db = &app.state.db;
        let alice = add_user(db, "Alice").await;
        let bob = add_user(db, "Bob").await;
        let t = add_transaction(db, NewTransaction::complete(alice, "2026-01-05", -1000, EUR)).await;
        add_shares(db, t, &[(alice, 50.0), (bob, 50.0)]).await;
        seed_rate(db, "USD", "EUR", "2026-01-05", 0.9).await;
        sqlx::query("INSERT INTO imported_files (filename, bank_id, source_file, importer_id, content_hash) \
                     VALUES ('releve.pdf', 1, 'Statements/BNP Paribas/releve.pdf', ?, 'abc')")
            .bind(alice).execute(db).await.unwrap();
        let statements = std::path::Path::new(&app.state.config.storage_path).join("Statements");
        std::fs::create_dir_all(statements.join("BNP Paribas")).unwrap();
        std::fs::write(statements.join("BNP Paribas/releve.pdf"), b"%PDF-1 %%EOF").unwrap();
        crate::db::set_setting(db, "default_currency_id", "3").await.unwrap();
        crate::db::set_setting(db, "log_level", "trace").await.unwrap();

        let counts = reset_everything(&app.state).await.unwrap();
        assert_eq!(counts, super::ResetCounts { users: 2, transactions: 1, files: 1 });

        let count = |table: &'static str| async move {
            sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(db).await.unwrap()
        };
        for table in ["users", "transactions", "transaction_shares", "imported_files", "exchange_rates"] {
            assert_eq!(count(table).await, 0, "{table}");
        }
        assert_eq!(count("categories").await, 19);
        assert_eq!(count("currencies").await, 3);
        let settings: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(db).await.unwrap();
        assert_eq!(settings, vec![("default_currency_id".into(), "1".into()), ("theme".into(), "light".into())]);
        // The PDFs are gone, the folder stays.
        assert!(statements.exists());
        assert_eq!(std::fs::read_dir(&statements).unwrap().count(), 0);
        // Ids start over: the next user created (the first-run admin) is user 1.
        assert_eq!(add_user(db, "Nouveau").await, 1);
        // The way back.
        let backups = std::fs::read_dir(&app.state.config.backup_dir).unwrap().count();
        assert_eq!(backups, 1);
    }
}
