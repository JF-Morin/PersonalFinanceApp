pub mod bnp_paribas;
pub mod credit_mutuel;

use crate::models::ParsedTransaction;
use anyhow::Result;

pub trait BankParser: Send + Sync {
    fn bank_id(&self) -> i64;
    fn bank_name(&self) -> &str;
    /// Returns true if the extracted PDF text looks like this bank's statement.
    fn detect(&self, text: &str) -> bool;
    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>>;
}

pub fn all_parsers() -> Vec<Box<dyn BankParser>> {
    vec![
        Box::new(bnp_paribas::BnpParibasParser),
        Box::new(credit_mutuel::CreditMutuelParser),
    ]
}

/// Auto-detect the bank from PDF text and return (bank_id, bank_name, transactions).
pub fn detect_and_parse(text: &str) -> Result<(i64, String, Vec<ParsedTransaction>)> {
    for parser in all_parsers() {
        tracing::debug!(parser = %parser.bank_name(), "Trying parser");
        if parser.detect(text) {
            tracing::info!(bank = %parser.bank_name(), "Parser matched, parsing transactions");
            let transactions = parser.parse(text)?;
            tracing::info!(
                bank         = %parser.bank_name(),
                transactions = %transactions.len(),
                "Parsing complete"
            );
            return Ok((parser.bank_id(), parser.bank_name().to_string(), transactions));
        }
    }
    tracing::warn!("No parser matched the uploaded document");
    anyhow::bail!("Aucun parser disponible pour ce document")
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Parse a European-formatted amount string like "1 234,56" or "1.234,56".
pub fn parse_european_amount(s: &str) -> Option<f64> {
    let cleaned = s
        .trim()
        .replace('\u{00a0}', "") // non-breaking space
        .replace(' ', "")
        .replace('.', "")
        .replace(',', ".");
    cleaned.parse::<f64>().ok()
}
