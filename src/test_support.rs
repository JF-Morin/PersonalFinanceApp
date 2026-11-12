/// Shared helpers for the tests: a temporary database (every migration applied), an
/// `AppState` around it, and small builders for users, transactions, shares and rates.
///
/// Each test gets its own folder (`TempDir`, deleted when the `TestApp` is dropped) with
/// the database file and the storage folder. The rate service is never called in tests
/// (`currency::fetch_series`): put the rates a test needs in `exchange_rates` with
/// `seed_rate`.
use std::sync::Arc;

use axum_extra::extract::cookie::{Cookie, Key, SignedCookieJar};
use tempfile::TempDir;

use crate::{config::Config, db::DbPool, AppState};

/// Currency ids seeded by migration 001.
pub const EUR: i64 = 1;
pub const USD: i64 = 2;
pub const CAD: i64 = 3;

pub struct TestApp {
    pub state: AppState,
    /// Holds the database and the storage folder; deleted on drop (never read: it only has
    /// to live as long as the test).
    #[allow(dead_code)]
    pub dir: TempDir,
}

/// A fresh database with every migration applied, in a temporary folder.
pub async fn test_db(dir: &TempDir) -> DbPool {
    let db = crate::db::create_pool(&database_url(dir)).await.expect("test database");
    crate::db::run_migrations(&db).await.expect("migrations");
    db
}

pub fn database_url(dir: &TempDir) -> String {
    format!("sqlite:{}", dir.path().join("finance.db").display())
}

/// An `AppState` over a fresh database (no templates: handlers tested here answer JSON
/// or toasts).
pub async fn test_app() -> TestApp {
    let dir = TempDir::new().expect("temp dir");
    let db = test_db(&dir).await;
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let config = Config {
        database_url: database_url(&dir),
        storage_path: storage.display().to_string(),
        log_path: dir.path().join("logs").display().to_string(),
        server_host: "127.0.0.1".into(),
        server_port: 0,
        cookie_secret: "x".repeat(32),
        backup_dir: dir.path().join("backups").display().to_string(),
        backup_keep: 1,
        backup_interval_hours: 0,
    };
    // A filter handle nobody reloads (the layer isn't installed).
    let (_layer, reload) = tracing_subscriber::reload::Layer::new(tracing_subscriber::EnvFilter::new("off"));
    let state = AppState {
        db,
        logs: Arc::new(crate::logs::LogControl::new(reload, &config.log_path)),
        config: Arc::new(config),
        tera: Arc::new(tera::Tera::default()),
        events: Arc::new(crate::sse::EventBus::new()),
        cookie_key: Key::from(&[7u8; 64]),
        uploads: Arc::new(crate::handlers::imports::PendingUploads::default()),
    };
    TestApp { state, dir }
}

/// The session cookie of `user_id`, as the login sets it.
pub fn jar_for(state: &AppState, user_id: i64) -> SignedCookieJar {
    SignedCookieJar::new(state.cookie_key.clone()).add(Cookie::new("user_id", user_id.to_string()))
}

// Inserts use `execute` + `last_insert_rowid()`, not `INSERT … RETURNING` with
// `fetch_one`: that leaves the statement unfinished on its pooled connection, which keeps
// the write lock and makes the next write elsewhere fail with "database is locked".

pub async fn add_user(db: &DbPool, name: &str) -> i64 {
    sqlx::query("INSERT INTO users (name, is_admin) VALUES (?, 0)")
        .bind(name)
        .execute(db)
        .await
        .unwrap()
        .last_insert_rowid()
}

/// A transaction as the app stores it. `rate`/`amount_cents` are the conversion to
/// `currency_id` (pass the original cents and 1.0 when it's the same currency).
pub struct NewTransaction {
    pub owner: i64,
    pub date: &'static str,
    pub original_cents: i64,
    pub original_currency: i64,
    pub rate: Option<f64>,
    pub amount_cents: i64,
    pub currency: i64,
    /// 1 pending, 2 complete, 3 broken.
    pub status: i64,
    pub category: Option<i64>,
}

impl NewTransaction {
    /// A complete transaction in `currency`, no conversion.
    pub fn complete(owner: i64, date: &'static str, cents: i64, currency: i64) -> Self {
        NewTransaction {
            owner,
            date,
            original_cents: cents,
            original_currency: currency,
            rate: Some(1.0),
            amount_cents: cents,
            currency,
            status: 2,
            category: Some(1),
        }
    }
}

pub async fn add_transaction(db: &DbPool, t: NewTransaction) -> i64 {
    sqlx::query(
        "INSERT INTO transactions (date, transaction_date, operation, original_amount_cents, \
         original_currency_id, exchange_rate, amount_cents, currency_id, category_id, owner_id, status_id) \
         VALUES (?, ?, 'test', ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(t.date)
    .bind(t.date)
    .bind(t.original_cents)
    .bind(t.original_currency)
    .bind(t.rate)
    .bind(t.amount_cents)
    .bind(t.currency)
    .bind(t.category)
    .bind(t.owner)
    .bind(t.status)
    .execute(db)
    .await
    .unwrap()
    .last_insert_rowid()
}

pub async fn add_shares(db: &DbPool, transaction_id: i64, shares: &[(i64, f64)]) {
    for (user_id, share) in shares {
        sqlx::query("INSERT INTO transaction_shares (transaction_id, user_id, share) VALUES (?, ?, ?)")
            .bind(transaction_id)
            .bind(user_id)
            .bind(share)
            .execute(db)
            .await
            .unwrap();
    }
}

/// A rate already known (as if fetched earlier), so no request is needed.
pub async fn seed_rate(db: &DbPool, from: &str, to: &str, date: &str, rate: f64) {
    sqlx::query("INSERT INTO exchange_rates (from_code, to_code, date, rate) VALUES (?, ?, ?, ?)")
        .bind(from)
        .bind(to)
        .bind(date)
        .bind(rate)
        .execute(db)
        .await
        .unwrap();
}
