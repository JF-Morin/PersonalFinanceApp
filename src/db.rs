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

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// A database as it was before migrations 007–010 (amounts as REAL, foreign amounts in
    /// their own columns, manual transactions on a synthetic 'manual' file), with shares:
    /// once migrated, every transaction and every share is still there and converted right.
    /// Guards the table rebuilds (007, 008), where dropping `transactions` would
    /// cascade-delete the shares.
    #[tokio::test]
    async fn old_data_survives_the_migrations() {
        let dir = tempfile::TempDir::new().unwrap();
        // Migrations 001–006 only: copied to a folder of their own.
        let old = dir.path().join("migrations-006");
        std::fs::create_dir(&old).unwrap();
        for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.as_str() < "007" {
                std::fs::copy(&path, old.join(&name)).unwrap();
            }
        }
        let db = super::create_pool(&crate::test_support::database_url(&dir)).await.unwrap();
        sqlx::migrate::Migrator::new(old.as_path()).await.unwrap().run(&db).await.unwrap();

        let old_data = [
            "INSERT INTO users (id, name, password_hash, is_admin) VALUES (1, 'Alice', NULL, 1), (2, 'Bob', NULL, 0)",
            "INSERT INTO imported_files (id, filename, bank_id, source_file, row_count, importer_id, content_hash) \
             VALUES (1, 'releve.pdf', 1, 'Statements/BNP Paribas/releve.pdf', 1, 1, 'abc'), (2, 'manual', 1, '', 0, 2, NULL)",
            // 1: a PDF line, −12.34 € (complete); 2: a manual −50 $ converted to −45 € at 0.9;
            // 3: a manual −20 $ whose rate couldn't be fetched (broken, amount 0).
            "INSERT INTO transactions (id, date, transaction_date, operation, amount, currency_id, category_id, \
             owner_id, status_id, from_file_id, notes, parse_note, foreign_currency_amount, foreign_currency_id) VALUES \
             (1, '2026-01-05', '2026-01-05', 'CB CAFE', -12.34, 1, 1, 1, 2, 1, NULL, NULL, NULL, NULL), \
             (2, '2026-01-06', '2026-01-06', 'NYC', -45.0, 1, 9, 2, 2, 2, 'voyage', NULL, -50.0, 2), \
             (3, '2026-01-07', '2026-01-07', 'TAXI', 0.0, 1, NULL, 2, 3, 2, NULL, 'Taux introuvable', -20.0, 2)",
            "INSERT INTO transaction_shares (transaction_id, user_id, share) VALUES (1, 1, 50), (1, 2, 50), (2, 2, 100)",
        ];
        for sql in old_data {
            sqlx::query(sql).execute(&db).await.unwrap();
        }

        super::run_migrations(&db).await.unwrap();

        let rows: Vec<(i64, i64, i64, Option<f64>, i64, i64, Option<i64>, Option<String>, i64)> = sqlx::query_as(
            "SELECT id, original_amount_cents, original_currency_id, exchange_rate, amount_cents, currency_id, \
                    from_file_id, notes, status_id FROM transactions ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows, vec![
            (1, -1234, 1, Some(1.0), -1234, 1, Some(1), None, 2),
            (2, -5000, 2, Some(0.9), -4500, 1, None, Some("voyage".into()), 2),
            (3, -2000, 2, None, 0, 1, None, None, 3),
        ]);
        let shares: Vec<(i64, i64, f64)> =
            sqlx::query_as("SELECT transaction_id, user_id, share FROM transaction_shares ORDER BY transaction_id, user_id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(shares, vec![(1, 1, 50.0), (1, 2, 50.0), (2, 2, 100.0)]);
        // The synthetic 'manual' file is gone (manual transactions have no file now), and
        // with it what kept Bob undeletable.
        let files: Vec<String> = sqlx::query_scalar("SELECT filename FROM imported_files").fetch_all(&db).await.unwrap();
        assert_eq!(files, vec!["releve.pdf".to_string()]);
        let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_active = 1").fetch_one(&db).await.unwrap();
        assert_eq!(active, 2);
        let broken_references: Vec<(String,)> = sqlx::query_as("PRAGMA foreign_key_check").fetch_all(&db).await.unwrap();
        assert!(broken_references.is_empty(), "{:?}", broken_references);
        // Deleting a transaction still cascades to its shares (the table was rebuilt).
        sqlx::query("DELETE FROM transactions WHERE id = 1").execute(&db).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transaction_shares").fetch_one(&db).await.unwrap();
        assert_eq!(left, 1);
    }
}
