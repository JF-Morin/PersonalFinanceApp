/// The user's local time.
///
/// Every page (`layout/shell.html`) stores the browser's UTC offset in a `tz_offset`
/// cookie (minutes east of UTC, refreshed on each page load, so it follows the user when
/// they travel or the clocks change). Handlers take `UserClock` as an extractor and use it
/// for "today", the current year, and to show UTC timestamps from the DB (`imported_at`,
/// `last_login`) in the user's time. Without the cookie (first request, JS off) it falls
/// back to the server's time zone (`TZ`).
use axum::{extract::FromRequestParts, http::{header, request::Parts}};
use chrono::{Duration, Local, NaiveDateTime, TimeZone, Utc};

/// Offsets beyond ±14 h don't exist: such a cookie is ignored.
const MAX_OFFSET_MINUTES: i64 = 14 * 60;
const DB_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

#[derive(Clone, Copy, Debug, Default)]
pub struct UserClock {
    /// Minutes east of UTC (UTC−5 = -300); `None` = the server's time zone.
    offset_minutes: Option<i64>,
}

#[axum::async_trait]
impl<S: Send + Sync> FromRequestParts<S> for UserClock {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let offset_minutes = parts
            .headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|c| c.trim().strip_prefix("tz_offset=").and_then(|v| v.parse::<i64>().ok()))
            .filter(|o| o.abs() <= MAX_OFFSET_MINUTES);
        Ok(UserClock { offset_minutes })
    }
}

impl UserClock {
    /// The user's current local date and time.
    pub fn now(&self) -> NaiveDateTime {
        match self.offset_minutes {
            Some(o) => Utc::now().naive_utc() + Duration::minutes(o),
            None => Local::now().naive_local(),
        }
    }

    /// Today, `YYYY-MM-DD`.
    pub fn today(&self) -> String {
        self.now().format("%Y-%m-%d").to_string()
    }

    /// The current year, `YYYY`.
    pub fn year(&self) -> String {
        self.now().format("%Y").to_string()
    }

    /// A UTC timestamp from the DB (`YYYY-MM-DD HH:MM:SS`, as SQLite's `CURRENT_TIMESTAMP`
    /// and `datetime('now')` write it) in the user's local time, same format. Anything
    /// else is returned unchanged.
    pub fn local(&self, utc: &str) -> String {
        let Ok(t) = NaiveDateTime::parse_from_str(utc, DB_FORMAT) else {
            return utc.to_string();
        };
        let local = match self.offset_minutes {
            Some(o) => t + Duration::minutes(o),
            None => Utc.from_utc_datetime(&t).with_timezone(&Local).naive_local(),
        };
        local.format(DB_FORMAT).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::UserClock;

    #[test]
    fn converts_utc_timestamps() {
        let montreal = UserClock { offset_minutes: Some(-300) };
        assert_eq!(montreal.local("2026-01-01 03:30:00"), "2025-12-31 22:30:00");
        let paris = UserClock { offset_minutes: Some(60) };
        assert_eq!(paris.local("2026-01-01 03:30:00"), "2026-01-01 04:30:00");
        assert_eq!(paris.local("not a date"), "not a date");
    }
}
