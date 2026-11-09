/// Shared pieces of the table filter system (transactions and imported files).
///
/// Each page has its own filter struct (query parameters, read with axum-extra's
/// `Query` so multi-choice filters can repeat their key: `?owner=1&owner=2`) and its
/// own filters modal. What they share lives here: parsing helpers, the query-string
/// encoding, and `FilterBar` — the context of `layout/_filter_bar.html`, the toolbar
/// line above the table (Filtres (n) button, the sort chip shown when the table is
/// cards, one removable chip per active filter, "Tout effacer", and the hidden inputs that carry the filters and the sort so the
/// search box submits them).
use serde::Serialize;

pub const MONTHS: [&str; 12] = [
    "Janvier", "Février", "Mars", "Avril", "Mai", "Juin",
    "Juillet", "Août", "Septembre", "Octobre", "Novembre", "Décembre",
];

/// Non-empty, trimmed text.
pub fn text(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// A four-digit year.
pub fn year(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|y| y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()))
}

/// A month number, 1–12.
pub fn month(s: &Option<String>) -> Option<u32> {
    s.as_deref().and_then(|m| m.parse().ok()).filter(|m| (1..=12).contains(m))
}

/// A `YYYY-MM-DD` date (what `<input type="date">` submits).
pub fn date(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|d| {
        let b = d.as_bytes();
        b.len() == 10 && b[4] == b'-' && b[7] == b'-'
            && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    })
}

/// A non-negative number (a comma is accepted as the decimal separator).
pub fn number(s: &Option<String>) -> Option<f64> {
    s.as_deref()
        .and_then(|a| a.trim().replace(',', ".").parse::<f64>().ok())
        .filter(|a| a.is_finite() && *a >= 0.0)
}

/// The numeric ids among `values`, sorted and without duplicates.
pub fn ids(values: &[String]) -> Vec<i64> {
    let mut ids: Vec<i64> = values.iter().filter_map(|v| v.parse().ok()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Drop the values of a multi-choice filter that point to nothing that still exists
/// (e.g. a user deleted by someone else since the filter was chosen), keeping the
/// special values in `keep` (like "none"). Done before anything reads the filter, so
/// the chips, the "Filtres (n)" count and the results always agree.
pub fn retain_known(values: &mut Vec<String>, known: &[i64], keep: &[&str]) {
    values.retain(|v| keep.contains(&v.as_str()) || v.parse::<i64>().map_or(false, |id| known.contains(&id)));
}

/// The first ten characters (`YYYY-MM-DD`) of a date or datetime.
pub fn day(datetime: &str) -> &str {
    datetime.get(..10).unwrap_or(datetime)
}

/// Whether `datetime` falls in the year / month / inclusive date range given.
pub fn in_period(datetime: &str, year: Option<&str>, month: Option<u32>, from: Option<&str>, to: Option<&str>) -> bool {
    year.map_or(true, |y| datetime.starts_with(y))
        && month.map_or(true, |m| datetime.get(5..7).and_then(|s| s.parse::<u32>().ok()) == Some(m))
        && from.map_or(true, |f| day(datetime) >= f)
        && to.map_or(true, |t| day(datetime) <= t)
}

/// "Du a au b" / "Depuis le a" / "Jusqu'au b" for a date range chip.
pub fn date_range_label(from: Option<&str>, to: Option<&str>) -> Option<String> {
    match (from, to) {
        (Some(a), Some(b)) => Some(format!("Du {} au {}", a, b)),
        (Some(a), None) => Some(format!("Depuis le {}", a)),
        (None, Some(b)) => Some(format!("Jusqu'au {}", b)),
        (None, None) => None,
    }
}

/// "<name> : a – b" / "<name> ≥ a" / "<name> ≤ b" for a number range chip.
pub fn number_range_label(name: &str, min: Option<String>, max: Option<String>) -> Option<String> {
    match (min, max) {
        (Some(a), Some(b)) => Some(format!("{} : {} – {}", name, a, b)),
        (Some(a), None) => Some(format!("{} ≥ {}", name, a)),
        (None, Some(b)) => Some(format!("{} ≤ {}", name, b)),
        (None, None) => None,
    }
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// `search` (when set) then `params`, as a query string.
pub fn query_string(search: Option<&str>, params: &[(&'static str, String)]) -> String {
    search
        .map(|s| ("search", s.to_string()))
        .into_iter()
        .chain(params.iter().cloned())
        .map(|(k, v)| format!("{}={}", k, url_encode(&v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// One active filter above the table.
#[derive(Serialize)]
pub struct FilterChip {
    pub icon: &'static str,
    pub label: String,
    /// Query string of the table without this filter.
    pub remove_query: String,
}

/// Builds the chips of a filter struct `F`: each chip's ✕ link is the current view
/// (`query` of a copy of the filter) with that one filter cleared.
pub struct Chips<'a, F: Clone> {
    filter: &'a F,
    query: fn(&F) -> String,
    pub chips: Vec<FilterChip>,
}

impl<'a, F: Clone> Chips<'a, F> {
    pub fn new(filter: &'a F, query: fn(&F) -> String) -> Self {
        Self { filter, query, chips: Vec::new() }
    }

    pub fn add(&mut self, icon: &'static str, label: String, clear: impl Fn(&mut F)) {
        let mut without = self.filter.clone();
        clear(&mut without);
        self.chips.push(FilterChip { icon, label, remove_query: (self.query)(&without) });
    }
}

/// Context of `layout/_filter_bar.html` (inserted as `filter_bar`).
#[derive(Serialize)]
pub struct FilterBar {
    /// Element id (the bar is swapped out-of-band with each table fragment).
    pub id: &'static str,
    /// Id of the search form the bar sits in (sent to the modal by the Filtres button).
    pub form: &'static str,
    /// Page URL that renders the table fragment, e.g. "/transactions".
    pub list_url: &'static str,
    /// Selector of the table fragment, e.g. "#transactions-table".
    pub target: &'static str,
    /// Selector of the table card, the `hx-sync` group of every request that reloads
    /// the table (see "Filterable tables" in CLAUDE.md).
    pub sync: &'static str,
    /// URL of the filters modal.
    pub modal_url: &'static str,
    /// The active filters (not the search) as hidden inputs.
    pub params: Vec<(&'static str, String)>,
    pub sort: &'static str,
    pub dir: &'static str,
    /// Header label of the sort column, for the sort chip shown in card mode.
    pub sort_label: &'static str,
    /// Query string of the same view sorted the other way (the sort chip's arrow).
    pub flip_query: String,
    pub chips: Vec<FilterChip>,
    /// Query string of the view with every filter removed (search and sort kept).
    pub cleared_query: String,
}

/// The opposite sort direction.
pub fn flip(dir: &str) -> &'static str {
    if dir == "asc" { "desc" } else { "asc" }
}

/// One choice of the filters modal's "Trier par" select (`modal/_sort_fields.html`).
#[derive(Serialize)]
pub struct SortChoice {
    pub key: &'static str,
    pub label: &'static str,
}
