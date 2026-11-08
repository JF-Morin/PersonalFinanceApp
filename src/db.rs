/// Database connection pool setup and helpers.
///
/// We use SQLite in WAL (Write-Ahead Log) mode with a small pool of connections.
/// WAL lets multiple readers run concurrently with a single writer, which is
/// important because HTMX fires several parallel requests on page load.
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

pub type DbPool = SqlitePool;

pub async fn create_pool(database_url: &str) -> anyhow::Result<DbPool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        // Create the database file on first run if it doesn't exist yet.
        .create_if_missing(true)
        // WAL allows concurrent readers without blocking writers (HTMX sends several requests per page).
        .pragma("journal_mode", "WAL")
        // SQLite ignores FK constraints by default; this pragma turns them on per connection.
        .pragma("foreign_keys", "ON");

    // 5 connections is enough for this single-user app.
    // Each connection gets its own WAL read snapshot, which is why a row inserted on
    // connection A may briefly be invisible to connection B (the "WAL snapshot" issue).
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    Ok(pool)
}

/// Run all pending SQL migrations from the `migrations/` directory.
/// sqlx::migrate! embeds the files at compile time so the binary is self-contained.
pub async fn run_migrations(pool: &DbPool) -> anyhow::Result<()> {
    tracing::info!("Running database migrations");
    sqlx::migrate!("./migrations").run(pool).await?;
    tracing::info!("Database migrations complete");
    Ok(())
}

/// Read a value from the `settings` key/value table.
/// Returns an error if the key doesn't exist — callers typically use `unwrap_or_else`.
pub async fn get_setting(pool: &DbPool, key: &str) -> anyhow::Result<String> {
    let row = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Upsert a value into the `settings` key/value table. Takes any executor, so it can run
/// on the pool or inside a DB transaction (`&mut *tx`).
pub async fn set_setting<'e, E: sqlx::SqliteExecutor<'e>>(executor: E, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
        .bind(key)
        .bind(value)
        .execute(executor)
        .await?;
    Ok(())
}
