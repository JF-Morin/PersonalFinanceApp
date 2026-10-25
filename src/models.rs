use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
    pub password_hash: Option<String>,
    pub last_login: Option<String>,
}

impl User {
    pub fn display_name(&self) -> String {
        match &self.last_name {
            Some(ln) => format!("{} {}", self.name, ln),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Bank {
    pub id: i64,
    pub name: String,
    pub logo: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Currency {
    pub id: i64,
    pub name: String,
    pub symbol: String,
    pub code: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Category {
    pub id: i64,
    pub name: String,
}


/// Intermediate struct produced by parsers before DB insertion.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ParsedTransaction {
    pub date: String,
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    #[serde(default)]
    pub parse_note: Option<String>,
}

/// A single user's share in a transaction.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ShareView {
    pub user_id: i64,
    pub share: f64,
    pub user_name: String,
    pub initials: String,
}

/// Enriched transaction for display (joins bank/category/user names + shares).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransactionView {
    pub id: i64,
    pub date: String,
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_symbol: String,
    pub currency_id: i64,
    pub category_name: Option<String>,
    pub category_id: Option<i64>,
    pub owner_name: String,
    pub owner_id: i64,
    pub shares: Vec<ShareView>,
    pub shares_total: f64,
    pub status_name: String,
    pub notes: Option<String>,
    pub from_file_id: i64,
    pub parse_note: Option<String>,
    pub foreign_currency_amount: Option<f64>,
    pub foreign_currency_id: Option<i64>,
    pub foreign_currency_symbol: Option<String>,
}

/// Per-user breakdown and net balance for a user pair + currency.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserPairBalance {
    pub user1_name: String,
    pub user1_total_debit: f64,
    pub user1_total_credit: f64,
    pub user1_debit_for_other: f64,
    pub user1_credit_for_other: f64,
    pub user1_total_for_other: f64,

    pub user2_name: String,
    pub user2_total_debit: f64,
    pub user2_total_credit: f64,
    pub user2_debit_for_other: f64,
    pub user2_credit_for_other: f64,
    pub user2_total_for_other: f64,

    pub debtor: String,
    pub creditor: String,
    pub net_amount: f64,
    pub currency_symbol: String,
}
