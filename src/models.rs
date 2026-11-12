/// Data models shared by all handlers and Tera templates.
///
/// Structs derived from `sqlx::FromRow` map directly from SQL result rows.
/// Structs derived from `Serialize` can be inserted into Tera template contexts.
/// Most structs derive both so they can go from DB → handler → template.
use serde::{Deserialize, Serialize};

/// A registered user.  `password_hash` is an Argon2 hash string; `None` means
/// the user has never logged in and must set a password on their first login.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub last_name: Option<String>,
    /// Default bank linked to this user — used to restrict which PDFs they can import.
    pub bank_id: Option<i64>,
    pub password_hash: Option<String>,
    pub last_login: Option<String>,
    /// The admin (first user of the app) is the only one who can deactivate or delete users.
    pub is_admin: bool,
    /// False once deactivated: can't log in, isn't offered as a participant; their data
    /// stays (migration 009).
    pub is_active: bool,
}

impl User {
    /// "Prénom Nom" or just "Prénom" if no last name is set, plus " (désactivé)" for a
    /// deactivated user.
    pub fn display_name(&self) -> String {
        let name = match self.last_name.as_deref().map(str::trim).filter(|ln| !ln.is_empty()) {
            Some(ln) => format!("{} {}", self.name, ln),
            None => self.name.clone(),
        };
        if self.is_active { name } else { format!("{} (désactivé)", name) }
    }
}

/// A financial institution (BNP, Desjardins, etc.).
/// Used to validate that a PDF belongs to the user's own bank before import.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Bank {
    pub id: i64,
    pub name: String,
    pub logo: Option<String>,
}

/// ISO currency (EUR, CAD, USD, …).
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Currency {
    pub id: i64,
    pub name: String,
    /// Display symbol, e.g. "€" or "CA$".
    pub symbol: String,
    /// ISO 4217 code used for exchange-rate API calls.
    pub code: String,
}

/// Spending category (Alimentation, Transport, …; the list is in migration 011).
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Category {
    pub id: i64,
    pub name: String,
}

/// Intermediate struct produced by bank parsers, before the transaction is written to the DB.
/// `parse_note` is non-None when the parser could not fully interpret the line
/// (amount or operation missing) — the whole file is then rejected. `amount` is in the
/// statement's currency (`BankParser::currency_code`); it becomes cents when stored.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ParsedTransaction {
    /// ISO date string "YYYY-MM-DD" — the posting / value date.
    pub date: String,
    /// The transaction (operation) date, which may differ from the posting date.
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    /// Set when the parser matched the line but couldn't extract all fields cleanly.
    #[serde(default)]
    pub parse_note: Option<String>,
}

/// One user's percentage share in a transaction, with display-ready name and initials.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ShareView {
    pub user_id: i64,
    /// Percentage, 0–100.
    pub share: f64,
    pub user_name: String,
    /// Two-letter initials for the avatar chip in the UI.
    pub initials: String,
}

/// Fully denormalized transaction ready for the Tera template.
/// Built by joining transactions with currencies, categories, users, shares, and status.
/// Kept separate from the DB row so the template never receives raw foreign keys.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransactionView {
    pub id: i64,
    /// ISO date shown in the table.
    pub date: String,
    /// May differ from `date` for bank-value-date vs posting-date.
    pub transaction_date: String,
    pub operation: String,
    /// Converted amount in the default currency (`amount_cents`), for display; sums
    /// and comparisons use `amount_cents`.
    pub amount: f64,
    pub amount_cents: i64,
    pub currency_symbol: String,
    pub currency_id: i64,
    /// What really happened: the statement line or what the user typed, in its own currency.
    pub original_amount: f64,
    pub original_amount_cents: i64,
    pub original_currency_id: i64,
    pub original_currency_symbol: String,
    /// Original currency → `currency_id` at the transaction date; `None` = no rate found.
    pub exchange_rate: Option<f64>,
    /// The original currency isn't the default one (show the original amount too).
    pub is_foreign: bool,
    pub category_name: Option<String>,
    pub category_id: Option<i64>,
    pub owner_name: String,
    pub owner_id: i64,
    /// All users who share this transaction, with their percentages.
    pub shares: Vec<ShareView>,
    /// Sum of all share percentages — used by the UI to show whether allocation is complete.
    pub shares_total: f64,
    /// "pending", "complete", or "broken".
    pub status_name: String,
    pub notes: Option<String>,
    /// The imported file it comes from; `None` for a manual transaction.
    pub from_file_id: Option<i64>,
    /// Added by hand (no file).
    pub is_manual: bool,
    /// Non-None for "broken" transactions — the raw parser note shown in the carousel.
    pub parse_note: Option<String>,
}

/// Balance summary for one (user1, user2, currency) triple.
/// The home page renders one card per entry.
/// `debtor` owes `net_amount` to `creditor` based on completed shared transactions.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserPairBalance {
    pub user1_name: String,
    /// Total of user1's negative (expense) transactions.
    pub user1_total_debit: f64,
    /// Total of user1's positive (income) transactions.
    pub user1_total_credit: f64,
    /// User1's share of expenses that involve user2.
    pub user1_debit_for_other: f64,
    /// User1's share of income that involves user2.
    pub user1_credit_for_other: f64,
    /// Net amount user1 owes to / is owed by user2 (debit_for_other + credit_for_other).
    pub user1_total_for_other: f64,

    pub user2_name: String,
    pub user2_total_debit: f64,
    pub user2_total_credit: f64,
    pub user2_debit_for_other: f64,
    pub user2_credit_for_other: f64,
    pub user2_total_for_other: f64,

    /// Name of the person who owes money (empty if balanced).
    pub debtor: String,
    /// Name of the person who is owed money (empty if balanced).
    pub creditor: String,
    /// Absolute net amount owed.
    pub net_amount: f64,
    pub currency_symbol: String,
}
