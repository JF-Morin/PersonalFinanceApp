use axum::{
    routing::{delete, get, post},
    Router,
};
use axum::extract::{DefaultBodyLimit, FromRef};
use axum_extra::extract::cookie::Key;
use std::sync::Arc;
use tera::Tera;
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod backup;
mod clock;
mod config;
mod currency;
mod db;
mod error;
mod handlers;
mod logs;
mod models;
mod parsers;
mod sse;
#[cfg(test)]
mod test_support;

use config::Config;
use db::DbPool;

/// Global application state — cloned cheaply per-request by Axum.
///
/// Every field is behind an `Arc` so cloning only increments a reference count.
/// Axum injects this via `State(state): State<AppState>` in handler arguments.
#[derive(Clone)]
pub struct AppState {
    /// SQLite connection pool — each `await` on a query borrows one connection.
    pub db: DbPool,
    /// Static config (ports, paths, secrets) loaded once at startup.
    pub config: Arc<Config>,
    /// Compiled Tera template engine — renders `.html` templates from `templates/`.
    pub tera: Arc<Tera>,
    /// Broadcast channel used to push SSE events to all connected browsers.
    pub events: Arc<sse::EventBus>,
    /// Signs the `user_id` session cookie (derived from COOKIE_SECRET), so a cookie
    /// edited or forged in the browser is rejected as "not logged in".
    pub cookie_key: Key,
    /// Analyzed PDFs waiting for their import confirmation (`handlers::imports`).
    pub uploads: Arc<handlers::imports::PendingUploads>,
    /// Live log level and log retention (admin settings, `logs.rs`).
    pub logs: Arc<logs::LogControl>,
}

/// Lets the `SignedCookieJar` extractor find the signing key in the app state.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.cookie_key.clone()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env first so LOG_PATH and RUST_LOG are available to Config
    dotenvy::dotenv().ok();

    let config = Config::from_env()?;

    // ── Logging setup ──────────────────────────────────────────────────────────
    // Create the log directory synchronously (logging not yet active)
    std::fs::create_dir_all(&config.log_path)?;

    // Daily rolling file: logs/app.log.YYYY-MM-DD
    let file_appender = tracing_appender::rolling::daily(&config.log_path, "app.log");
    let (non_blocking_file, _guard) = tracing_appender::non_blocking(file_appender);
    // _guard must stay alive for the whole program — dropping it flushes and stops the writer

    // RUST_LOG (or the built-in default) until the admin's saved level is applied, once
    // the DB is open (`logs::start`). The reload handle lets that level change at runtime.
    let (log_filter, log_reload) = tracing_subscriber::reload::Layer::new(logs::env_filter());
    let log_control = Arc::new(logs::LogControl::new(log_reload, &config.log_path));

    tracing_subscriber::registry()
        .with(log_filter)
        // Stdout: human-readable with colours, for `docker logs`
        .with(tracing_subscriber::fmt::layer())
        // File: same format, ANSI codes stripped so the file is clean text
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking_file)
                .with_ansi(false),
        )
        .init();

    tracing::info!(
        log_dir = %config.log_path,
        port    = %config.server_port,
        "Starting personal-finance-app"
    );

    // ── Directories ────────────────────────────────────────────────────────────
    // Ensure the SQLite database directory exists before sqlx tries to open the file.
    if let Some(parent) = std::path::Path::new(&config.database_url.replace("sqlite:", "")).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::create_dir_all(format!("{}/Statements", config.storage_path)).await.ok();
    tokio::fs::create_dir_all(format!("{}/Images", config.storage_path)).await.ok();

    // ── Database ───────────────────────────────────────────────────────────────
    let db = db::create_pool(&config.database_url).await?;
    // Embedded migrations (from `migrations/`) are run automatically at startup.
    // Back up before migrations, so an update can be rolled back to the schema it started from.
    backup::start(&db, &config).await;
    db::run_migrations(&db).await?;
    // Admin log settings: saved level, and deletion of old log files.
    logs::start(&db, log_control.clone()).await;

    // ── Templates ─────────────────────────────────────────────────────────────
    // `Tera::new` compiles all templates from the glob at startup — a parse error here
    // crashes the server immediately rather than at render time.
    let mut tera = Tera::new("templates/**/*.html")?;
    // Custom filter: `{{ value | fixed2 }}` formats a float to exactly 2 decimal places.
    tera.register_filter("fixed2", |value: &tera::Value, _: &std::collections::HashMap<String, tera::Value>| {
        match value.as_f64() {
            Some(f) => Ok(tera::Value::String(format!("{:.2}", f))),
            None => Err(tera::Error::msg("fixed2: expected a number")),
        }
    });
    tracing::info!("Templates loaded");

    let state = AppState {
        db,
        config: Arc::new(config.clone()),
        tera: Arc::new(tera),
        events: Arc::new(sse::EventBus::new()),
        // Key needs 64 bytes; SHA-512 stretches the (≥ 32-byte) secret to exactly that.
        cookie_key: Key::from(&<sha2::Sha512 as sha2::Digest>::digest(config.cookie_secret.as_bytes())),
        uploads: Arc::new(handlers::imports::PendingUploads::default()),
        logs: log_control,
    };

    // ── Router ─────────────────────────────────────────────────────────────────
    let app = Router::new()
        // ── Auth ──────────────────────────────────────────────────────────────
        .route("/login",  get(handlers::auth::get_login).post(handlers::auth::post_login))
        .route("/login/users/:id", get(handlers::auth::get_login_modal))
        .route("/logout", post(handlers::auth::post_logout))
        // ── Home page (balances, chart, stats) ────────────────────────────────
        .route("/",                        get(handlers::home::get_home))
        .route("/home/balances",           get(handlers::home::get_balances_partial))
        .route("/home/chart-part",         get(handlers::home::get_chart_part))
        .route("/home/stats-row",          get(handlers::home::get_stats_row))
        // ── Settings ──────────────────────────────────────────────────────────
        .route("/settings",                get(handlers::settings::get_settings)
                                          .post(handlers::settings::post_settings))
        .route("/settings/logs",           post(handlers::settings::post_log_settings))
        .route("/settings/reset",          get(handlers::settings::get_reset_modal)
                                          .post(handlers::settings::post_reset))
        .route("/settings/backups/delete", get(handlers::settings::get_delete_backups_modal)
                                          .post(handlers::settings::post_delete_backups))
        .route("/settings/logs/delete",    get(handlers::settings::get_delete_logs_modal)
                                          .post(handlers::settings::post_delete_logs))
        .route("/settings/users",          get(handlers::settings::get_users_table)
                                          .post(handlers::settings::post_new_user))
        .route("/settings/users/:id",      delete(handlers::settings::delete_user))
        .route("/settings/users/:id/delete", get(handlers::settings::get_delete_user_modal))
        .route("/settings/users/:id/deactivate", post(handlers::settings::post_deactivate_user))
        .route("/settings/users/:id/reactivate", post(handlers::settings::post_reactivate_user))
        .route("/settings/users/:id/edit", get(handlers::settings::get_edit_user_modal)
                                          .post(handlers::settings::post_edit_user))
        // ── Transactions ──────────────────────────────────────────────────────
        .route("/transactions",               get(handlers::transactions::get_transactions))
        .route("/transactions/export.csv",    get(handlers::transactions::get_export_csv))
        .route("/transactions/filters",       get(handlers::transactions::get_filters_modal))
        .route("/transactions/actions",       get(handlers::transactions::get_actions_modal))
        .route("/transactions/new",           get(handlers::transactions::get_manual_modal))
        .route("/transactions/manual",        post(handlers::transactions::post_manual_transaction))
        .route("/transactions/pending-badge", get(handlers::transactions::get_pending_badge))
        .route("/transactions/pending",       get(handlers::transactions::get_carousel))
        .route("/transactions/pending/:id",   post(handlers::transactions::post_carousel))
        .route("/transactions/:id/edit",      get(handlers::transactions::get_edit_modal))
        .route("/transactions/:id/update",    post(handlers::transactions::post_update_transaction))
        .route("/transactions/:id/delete",    get(handlers::transactions::get_delete_modal)
                                             .post(handlers::transactions::post_delete_transaction))
        // ── Imports ───────────────────────────────────────────────────────────
        .route("/imported-files",          get(handlers::imports::get_imported_files))
        .route("/imported-files/filters",  get(handlers::imports::get_file_filters_modal))
        .route("/imported-files/:id/delete",  get(handlers::imports::get_delete_file_modal)
                                             .post(handlers::imports::post_delete_imported_file))
        .route("/imported-files/:id/reparse", get(handlers::imports::get_reparse_file_modal)
                                             .post(handlers::imports::post_reparse_imported_file))
        .route("/upload",                  get(handlers::imports::get_import_modal))
        // PDFs up to MAX_UPLOAD_BYTES (plus room for the multipart framing); axum's
        // default limit is 2 MB.
        .route("/upload/analyze",          post(handlers::imports::post_analyze)
                                          .layer(DefaultBodyLimit::max(handlers::imports::MAX_UPLOAD_BYTES + 64 * 1024)))
        .route("/upload/confirm",          post(handlers::imports::post_confirm_import))
        // ── Misc ──────────────────────────────────────────────────────────────
        .route("/404-not-found",           get(handlers::get_not_found))
        // SSE endpoint — browsers hold this connection open for the lifetime of the tab.
        .route("/events",                  get(sse::get_events))
        // Static files served directly from the `static/` directory.
        .nest_service("/static", ServeDir::new("static"))
        // Catch-all: redirect unknown routes to the 404 page.
        .fallback(handlers::handler_404)
        // Sessions of deleted / deactivated users end (see handlers::require_active_user).
        .layer(axum::middleware::from_fn_with_state(state.clone(), handlers::require_active_user))
        // Redirects answering HTMX requests become HX-Redirect (see handlers::htmx_redirects);
        // added after the layer above, so it also wraps it.
        .layer(axum::middleware::from_fn(handlers::htmx_redirects))
        // HTTP request/response tracing — logs method, path, status, latency
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state);

    let addr = format!("{}:{}", config.server_host, config.server_port);
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
