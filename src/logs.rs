/// Log level and log retention, both admin settings (settings page, `POST /settings/logs`).
///
/// - Level: the filter is a `reload` layer, so a new level applies at once, without a
///   restart. Saved as `log_level` in `settings`; without it (or "" = "par défaut") the
///   `RUST_LOG` environment variable / the built-in default applies.
/// - Retention: the daily files `app.log.YYYY-MM-DD` older than `log_keep_days` days are
///   deleted — at startup, every few hours, and when the setting is saved. 0 keeps them all.
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing_subscriber::{reload, EnvFilter, Registry};

use crate::db::DbPool;

/// Levels offered on the settings page, least to most verbose.
pub const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
/// Days of logs kept when nothing is saved.
pub const DEFAULT_KEEP_DAYS: u32 = 30;
const DEFAULT_FILTER: &str = "personal_finance_app=debug,tower_http=info";
const FILE_PREFIX: &str = "app.log.";

/// The filter used before any admin setting: `RUST_LOG`, or the built-in default.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into())
}

/// The filter for a level chosen on the settings page: the app at that level, the HTTP
/// request lines (tower_http) at `info` — or quieter if the level is.
fn level_filter(level: &str) -> EnvFilter {
    let http = if matches!(level, "error" | "warn") { level } else { "info" };
    EnvFilter::new(format!("personal_finance_app={},tower_http={}", level, http))
}

/// Changes the live log filter and prunes old log files.
pub struct LogControl {
    handle: reload::Handle<EnvFilter, Registry>,
    dir: PathBuf,
}

impl LogControl {
    pub fn new(handle: reload::Handle<EnvFilter, Registry>, dir: impl Into<PathBuf>) -> Self {
        LogControl { handle, dir: dir.into() }
    }

    /// Apply a level from `LEVELS`, or "" for the default (`RUST_LOG`).
    pub fn set_level(&self, level: &str) -> anyhow::Result<()> {
        let filter = if level.is_empty() { env_filter() } else { level_filter(level) };
        self.handle.reload(filter)?;
        Ok(())
    }

    /// Delete the daily log files older than `keep_days` days (today's file counts as
    /// one); 0 keeps everything. Returns how many files were deleted.
    pub fn prune(&self, keep_days: u32) -> usize {
        if keep_days == 0 {
            return 0;
        }
        // tracing-appender names the files after the UTC date.
        let oldest_kept = chrono::Utc::now().date_naive() - chrono::Duration::days(i64::from(keep_days) - 1);
        let mut deleted = 0;
        for name in log_files(&self.dir) {
            let date = name.strip_prefix(FILE_PREFIX).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            if date.is_some_and(|d| d < oldest_kept) {
                match std::fs::remove_file(self.dir.join(&name)) {
                    Ok(()) => deleted += 1,
                    Err(e) => tracing::warn!(file = %name, error = %e, "Could not delete old log file"),
                }
            }
        }
        if deleted > 0 {
            tracing::info!(deleted, keep_days, "Old log files deleted");
        }
        deleted
    }
}

fn log_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(FILE_PREFIX))
                .collect()
        })
        .unwrap_or_default()
}

/// The saved settings: (level — "" = default, days to keep).
pub async fn saved(db: &DbPool) -> (String, u32) {
    let level = crate::db::get_setting(db, "log_level").await.unwrap_or_default();
    let level = if LEVELS.contains(&level.as_str()) { level } else { String::new() };
    let keep_days = crate::db::get_setting(db, "log_keep_days")
        .await
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_KEEP_DAYS);
    (level, keep_days)
}

/// At startup: apply the saved level, prune, and keep pruning every 6 hours (so files
/// age out even when the app runs for weeks).
pub async fn start(db: &DbPool, control: std::sync::Arc<LogControl>) {
    let (level, keep_days) = saved(db).await;
    if let Err(e) = control.set_level(&level) {
        tracing::warn!(error = %e, "Could not apply the saved log level");
    }
    control.prune(keep_days);
    let db = db.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(6 * 3600));
        ticker.tick().await; // immediate — done just above
        loop {
            ticker.tick().await;
            let (_, keep_days) = saved(&db).await;
            control.prune(keep_days);
        }
    });
}
