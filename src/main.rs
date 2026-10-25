use axum::{
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;
use tera::Tera;
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod currency;
mod db;
mod error;
mod handlers;
mod models;
mod parsers;

use config::Config;
use db::DbPool;

#[derive(Clone)]
pub struct AppState {
    pub db: DbPool,
    pub config: Arc<Config>,
    pub tera: Arc<Tera>,
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

    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "personal_finance_app=debug,tower_http=info".into());

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
    if let Some(parent) = std::path::Path::new(&config.database_url.replace("sqlite:", "")).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::create_dir_all(format!("{}/Statements", config.storage_path)).await.ok();
    tokio::fs::create_dir_all(format!("{}/Images", config.storage_path)).await.ok();

    // ── Database ───────────────────────────────────────────────────────────────
    let db = db::create_pool(&config.database_url).await?;
    db::run_migrations(&db).await?;

    // ── Templates ─────────────────────────────────────────────────────────────
    let mut tera = Tera::new("templates/**/*.html")?;
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
    };

    // ── Router ─────────────────────────────────────────────────────────────────
    let app = Router::new()
        // Auth
        .route("/login",  get(handlers::auth::get_login).post(handlers::auth::post_login))
        .route("/logout", post(handlers::auth::post_logout))
        // Main pages
        .route("/",                        get(handlers::home::get_home))
        .route("/home/balances",           get(handlers::home::get_balances_partial))
        .route("/home/chart-data",         get(handlers::home::get_chart_data))
        .route("/settings",                get(handlers::settings::get_settings)
                                          .post(handlers::settings::post_settings))
        .route("/settings/users",          post(handlers::settings::post_new_user))
        .route("/settings/users/:id",      delete(handlers::settings::delete_user))
        .route("/settings/users/:id/edit", post(handlers::settings::post_edit_user))
        .route("/settings/password",       post(handlers::settings::post_change_password))
        // Transactions
        .route("/transactions",            get(handlers::transactions::get_transactions))
        .route("/transactions/:id/update", post(handlers::transactions::post_update_transaction))
        .route("/transactions/:id/delete", post(handlers::transactions::post_delete_transaction))
        .route("/transactions/:id/shares", post(handlers::transactions::post_transaction_share))
        .route("/transactions/:id/shares/:user_id", delete(handlers::transactions::delete_transaction_share))
        .route("/transactions/manual",     post(handlers::transactions::post_manual_transaction))
        // Imports
        .route("/imported-files",          get(handlers::imports::get_imported_files))
        .route("/imported-files/:id/delete",  post(handlers::imports::post_delete_imported_file))
        .route("/imported-files/:id/reparse", post(handlers::imports::post_reparse_imported_file))
        .route("/upload/analyze",          post(handlers::imports::post_analyze))
        .route("/upload/confirm",          post(handlers::imports::post_confirm_import))
        .route("/404-not-found",           get(handlers::get_not_found))
        // Static files
        .nest_service("/static", ServeDir::new("static"))
        // 404 fallback for undefined routes
        .fallback(handlers::handler_404)
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
