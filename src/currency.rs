/// Money and exchange rates.
///
/// Amounts are stored as integer cents (`*_cents` columns). Every transaction keeps its
/// original amount and currency; `amount_cents` is that amount converted to the default
/// currency with `exchange_rate`, the rate of the transaction date (see migration 007).
///
/// Rates come from the Frankfurter API (European Central Bank rates, published on working
/// days only). They are fetched up front — one request per currency for the whole date
/// range needed — and never while a DB transaction is open.
use std::collections::{BTreeMap, HashMap};

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

/// Rates from any currency to one target currency, fetched once for a set of
/// (currency, date) pairs.
pub struct Rates {
    to_id: i64,
    series: HashMap<i64, RateSeries>,
}

impl Rates {
    /// Fetch the rates to `to_id` that `needs` (currency id, date) require: one request
    /// per currency other than `to_id`. Fails if a currency has no ISO code or a request
    /// fails, naming the currency.
    pub async fn fetch(db: &DbPool, to_id: i64, needs: &[(i64, String)]) -> anyhow::Result<Rates> {
        // Date range needed per currency.
        let mut ranges: HashMap<i64, (String, String)> = HashMap::new();
        for (currency_id, date) in needs.iter().filter(|(c, _)| *c != to_id) {
            let range = ranges.entry(*currency_id).or_insert_with(|| (date.clone(), date.clone()));
            if *date < range.0 { range.0 = date.clone(); }
            if *date > range.1 { range.1 = date.clone(); }
        }
        let mut series = HashMap::new();
        if ranges.is_empty() {
            return Ok(Rates { to_id, series });
        }
        let to_code = currency_code(db, to_id).await?;
        for (currency_id, (start, end)) in ranges {
            let from_code = currency_code(db, currency_id).await?;
            let rates = fetch_series(&from_code, &to_code, &start, &end)
                .await
                .with_context(|| format!("taux {} → {} introuvables", from_code, to_code))?;
            tracing::info!(from = %from_code, to = %to_code, start = %start, end = %end, days = rates.0.len(), "Exchange rates fetched");
            series.insert(currency_id, rates);
        }
        Ok(Rates { to_id, series })
    }

    /// The rate from `currency_id` to the target currency on `date` (1 for the target
    /// itself); `None` if it wasn't fetched.
    pub fn rate(&self, currency_id: i64, date: &str) -> Option<f64> {
        if currency_id == self.to_id {
            return Some(1.0);
        }
        self.series.get(&currency_id)?.on(date)
    }
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
    }
}
