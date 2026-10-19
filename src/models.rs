use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub last_name: Option<String>,
    pub bank_id: Option<i64>,
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
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Category {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct TransactionStatus {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct ImportedFile {
    pub id: i64,
    pub filename: String,
    pub bank_id: i64,
    pub source_file: String,
    pub imported_at: String,
    pub row_count: i64,
    pub importer_id: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Transaction {
    pub id: i64,
    pub date: String,
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_id: i64,
    pub category_id: Option<i64>,
    pub paid_by: i64,
    pub other_share: Option<f64>,
    pub status_id: i64,
    pub from_file_id: i64,
    pub notes: Option<String>,
}

/// Intermediate struct produced by parsers before DB insertion.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ParsedTransaction {
    pub date: String,
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    // serde(default) so the JSON round-trip in the import-confirm form survives a missing key.
    #[serde(default)]
    pub parse_note: Option<String>,
}

/// Enriched transaction for display (joins bank/category/user names).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransactionView {
    pub id: i64,
    pub date: String,
    pub transaction_date: String,
    pub operation: String,
    pub amount: f64,
    pub currency_symbol: String,
    pub category_name: Option<String>,
    pub paid_by_name: String,
    pub other_share: Option<f64>,
    pub status_name: String,
    pub notes: Option<String>,
    pub from_file_id: i64,
    pub parse_note: Option<String>, // non-null means the transaction still needs manual correction
}

/// Balance owed between two users.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Balance {
    pub from_user: String,
    pub to_user: String,
    pub amount: f64,
    pub currency_symbol: String,
}
