/// Money and exchange rates.
///
/// Amounts are stored as integer cents (`*_cents` columns). Every transaction keeps its
/// original amount and currency; `amount_cents` is that amount converted to the default
/// currency with `exchange_rate`, the rate of the transaction date (see migration 007).
///
/// Rates come from the Frankfurter API (European Central Bank rates, published on working
/// days only). They are fetched up front — one request per currency for the whole date
/// range needed — and never while a DB transaction is open. Every final rate fetched is
/// kept in `exchange_rates` (migration 010), so a (currency, date) already seen is never
/// asked again.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{anyhow, bail, Context};

use crate::db::DbPool;

/// `12.34` → `1234`.
pub fn to_cents(amount: f64) -> i64 {
    (amount * 100.0).round() as i64
}

/// `1234` → `12.34` (for display and parsing user input only — sums stay in cents).
pub fn from_cents(cents: i64) -> f64 {
    cents as f64 / 100.0
}

/// An amount in cents converted with `rate`, rounded to the cent.
pub fn convert(cents: i64, rate: f64) -> i64 {
    (cents as f64 * rate).round() as i64
}

/// Daily rates from one currency to another, by date (`YYYY-MM-DD`).
struct RateSeries(BTreeMap<String, f64>);

impl RateSeries {
    /// The rate of `date`, or of the last working day before it (weekends, holidays, and
    /// today before the ECB publishes).
    fn on(&self, date: &str) -> Option<f64> {
        self.0.range(..=date.to_string()).next_back().map(|(_, r)| *r)
    }

    /// The rate of `date` can't change any more: a rate was published on that date or a
    /// later one. (Today before the ECB publishes, or a weekend at the end of the series,
    /// still use an earlier rate that the next publication would replace.)
    fn is_final(&self, date: &str) -> bool {
        self.0.range(date.to_string()..).next().is_some()
    }
}

/// Rates from `from_code` to `to_code` for every working day from `start` to `end`. The
/// series starts on the last working day before `start`, so every date in the range has a
/// rate.
async fn fetch_series(from_code: &str, to_code: &str, start: &str, end: &str) -> anyhow::Result<RateSeries> {
    let url = format!("https://api.frankfurter.dev/v1/{}..{}?from={}&to={}", start, end, from_code, to_code);
    let resp: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let days = resp["rates"].as_object().ok_or_else(|| anyhow!("no rates in the response"))?;
    let mut series = BTreeMap::new();
    for (date, rates) in days {
        if let Some(rate) = rates[to_code].as_f64().filter(|r| r.is_finite() && *r > 0.0) {
            series.insert(date.clone(), rate);
        }
    }
    if series.is_empty() {
        bail!("no {} → {} rate between {} and {}", from_code, to_code, start, end);
    }
    Ok(RateSeries(series))
}

/// Rates from any currency to one target currency, for a set of (currency, date) pairs:
/// from `exchange_rates` when already known, otherwise fetched (and then stored).
pub struct Rates {
    to_id: i64,
    /// (currency id, date) → rate.
    rates: HashMap<(i64, String), f64>,
}

impl Rates {
    /// The rates to `to_id` that `needs` (currency id, date) require. For each currency
    /// other than `to_id`, the dates not in `exchange_rates` are fetched in one request.
    /// Fails if a currency has no ISO code or a needed request fails, naming the currency.
    pub async fn fetch(db: &DbPool, to_id: i64, needs: &[(i64, String)]) -> anyhow::Result<Rates> {
        let mut dates: HashMap<i64, BTreeSet<String>> = HashMap::new();
        for (currency_id, date) in needs.iter().filter(|(c, _)| *c != to_id) {
            dates.entry(*currency_id).or_default().insert(date.clone());
        }
        let mut rates = HashMap::new();
        if dates.is_empty() {
            return Ok(Rates { to_id, rates });
        }
        let to_code = currency_code(db, to_id).await?;
        for (currency_id, dates) in dates {
            let from_code = currency_code(db, currency_id).await?;
            let (first, last) = (dates.first().unwrap().clone(), dates.last().unwrap().clone());

            // Already known.
            let known: Vec<(String, f64)> = sqlx::query_as(
                "SELECT date, rate FROM exchange_rates \
                 WHERE from_code = ? AND to_code = ? AND date BETWEEN ? AND ?",
            )
            .bind(&from_code)
            .bind(&to_code)
            .bind(&first)
            .bind(&last)
            .fetch_all(db)
            .await?;
            let known: HashMap<String, f64> = known.into_iter().collect();
            let missing: Vec<&String> = dates.iter().filter(|d| !known.contains_key(*d)).collect();
            for (date, rate) in known.into_iter().filter(|(d, _)| dates.contains(d)) {
                rates.insert((currency_id, date), rate);
            }
            if missing.is_empty() {
                tracing::debug!(from = %from_code, to = %to_code, dates = dates.len(), "Exchange rates all known");
                continue;
            }

            // The rest: one request covering them — and a week beyond the last one (up to
            // today), so a weekend or holiday is seen to be followed by a published rate:
            // its rate is then final and can be stored.
            let start = missing[0].as_str();
            let last = missing[missing.len() - 1].as_str();
            let today = chrono::Utc::now().date_naive();
            let end = chrono::NaiveDate::parse_from_str(last, "%Y-%m-%d")
                .map(|d| (d + chrono::Duration::days(7)).min(today).max(d))
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|_| last.to_string());
            let series = fetch_series(&from_code, &to_code, start, &end)
                .await
                .with_context(|| format!("taux {} → {} introuvables", from_code, to_code))?;
            let mut to_store: Vec<(&str, f64)> = Vec::new();
            for date in &missing {
                if let Some(rate) = series.on(date) {
                    rates.insert((currency_id, date.to_string()), rate);
                    if series.is_final(date) {
                        to_store.push((date.as_str(), rate));
                    }
                }
            }
            tracing::info!(
                from = %from_code, to = %to_code, start = %start, end = %end,
                known = dates.len() - missing.len(), fetched = missing.len(), stored = to_store.len(),
                "Exchange rates fetched"
            );
            // Keeping them is an optimisation: a failure is logged, the conversion goes on.
            if let Err(e) = store(db, &from_code, &to_code, &to_store).await {
                tracing::warn!(error = %e, "Could not store exchange rates");
            }
        }
        Ok(Rates { to_id, rates })
    }

    /// The rate from `currency_id` to the target currency on `date` (1 for the target
    /// itself); `None` if it wasn't found.
    pub fn rate(&self, currency_id: i64, date: &str) -> Option<f64> {
        if currency_id == self.to_id {
            return Some(1.0);
        }
        self.rates.get(&(currency_id, date.to_string())).copied()
    }
}

/// Keep final rates in `exchange_rates` (one DB transaction).
async fn store(db: &DbPool, from_code: &str, to_code: &str, rates: &[(&str, f64)]) -> anyhow::Result<()> {
    if rates.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await?;
    for (date, rate) in rates {
        sqlx::query("INSERT OR IGNORE INTO exchange_rates (from_code, to_code, date, rate) VALUES (?, ?, ?, ?)")
            .bind(from_code)
            .bind(to_code)
            .bind(date)
            .bind(rate)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// ISO 4217 code of a currency (the one Frankfurter knows it by).
async fn currency_code(db: &DbPool, currency_id: i64) -> anyhow::Result<String> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT name, code FROM currencies WHERE id = ?")
        .bind(currency_id)
        .fetch_optional(db)
        .await?;
    match row {
        Some((_, code)) if !code.is_empty() => Ok(code),
        Some((name, _)) => bail!("la devise {} n'a pas de code ISO", name),
        None => bail!("devise {} inconnue", currency_id),
    }
}

/// The currency id with this ISO code, if the app knows it.
pub async fn currency_id_by_code(db: &DbPool, code: &str) -> anyhow::Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM currencies WHERE code = ?")
        .bind(code)
        .fetch_optional(db)
        .await?)
}

/// The default currency (app preference).
pub async fn default_currency_id(db: &DbPool) -> i64 {
    crate::db::get_setting(db, "default_currency_id")
        .await
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents() {
        assert_eq!(to_cents(-12.34), -1234);
        assert_eq!(to_cents(0.1 + 0.2), 30);
        assert_eq!(from_cents(-1234), -12.34);
        assert_eq!(convert(-10000, 0.85317), -8532);
    }

    #[test]
    fn rate_falls_back_to_the_last_working_day() {
        let series = RateSeries(BTreeMap::from([
            ("2026-01-02".to_string(), 0.85),
            ("2026-01-05".to_string(), 0.86),
        ]));
        assert_eq!(series.on("2026-01-02"), Some(0.85));
        assert_eq!(series.on("2026-01-04"), Some(0.85)); // Sunday → Friday
        assert_eq!(series.on("2026-01-09"), Some(0.86));
        assert_eq!(series.on("2025-12-31"), None);
        // Final once a rate exists on or after the date.
        assert!(series.is_final("2026-01-02"));
        assert!(series.is_final("2026-01-04"));
        assert!(!series.is_final("2026-01-09"));
    }
}
