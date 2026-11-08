/// Database backups.
///
/// The database runs in WAL mode: recent writes live in `finance.db-wal` until SQLite
/// folds them into `finance.db`, so copying `finance.db` alone (as the old entrypoint
/// script did) can miss the latest changes. `VACUUM INTO` writes a complete, consistent
/// snapshot to a new file instead — and it works while the app keeps running.
///
/// A backup is made at startup, before migrations (so an update can always be rolled
/// back to the schema it started from), and then every `BACKUP_INTERVAL_HOURS`.
/// Only the `BACKUP_KEEP` most recent `finance_YYYYMMDD_HHMMSS.db` files are kept.
///
/// Restore: stop the app, replace `finance.db` with a backup file (and delete
/// `finance.db-wal` / `finance.db-shm` if present), start the app.
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{config::Config, db::DbPool};

/// Write one snapshot to `dir` and prune old ones. Returns the new file's path.
pub async fn backup(db: &DbPool, dir: &Path, keep: usize, reason: &str) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let stamp: String = sqlx::query_scalar("SELECT strftime('%Y%m%d_%H%M%S', 'now', 'localtime')")
        .fetch_one(db)
        .await?;
    let path = dir.join(format!("finance_{}.db", stamp));
    if tokio::fs::try_exists(&path).await? {
        return Ok(path); // a backup was already written this second
    }
    sqlx::query("VACUUM INTO ?")
        .bind(path.to_string_lossy().to_string())
        .execute(db)
        .await?;
    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    tracing::info!(path = %path.display(), bytes = size, reason, "Database backup written");
    prune(dir, keep).await?;
    Ok(path)
}

/// Delete all but the `keep` newest backups (names sort by their timestamp).
async fn prune(dir: &Path, keep: usize) -> anyhow::Result<()> {
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("finance_") && name.ends_with(".db") {
            names.push(name);
        }
    }
    names.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    for old in names.iter().skip(keep) {
        tokio::fs::remove_file(dir.join(old)).await?;
        tracing::debug!(file = %old, "Old backup removed");
    }
    Ok(())
}

/// Startup backup + a background task repeating it every `backup_interval_hours`.
/// A failed backup is logged; it never stops the app.
pub async fn start(db: &DbPool, config: &Config) {
    let dir = PathBuf::from(&config.backup_dir);
    let keep = config.backup_keep;
    if let Err(e) = backup(db, &dir, keep, "startup").await {
        tracing::error!(error = %e, dir = %dir.display(), "Startup database backup FAILED");
    }
    if config.backup_interval_hours == 0 {
        return; // periodic backups disabled
    }
    let db = db.clone();
    let every = Duration::from_secs(config.backup_interval_hours * 3600);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await; // the first tick is immediate — the startup backup covers it
        loop {
            ticker.tick().await;
            if let Err(e) = backup(&db, &dir, keep, "scheduled").await {
                tracing::error!(error = %e, dir = %dir.display(), "Scheduled database backup FAILED");
            }
        }
    });
}
