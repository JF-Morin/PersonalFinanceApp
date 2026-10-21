pub mod bnp_paribas;
pub mod credit_mutuel;
pub mod lopdf_text;

use crate::models::ParsedTransaction;
use anyhow::Result;

pub trait BankParser: Send + Sync {
    fn bank_id(&self) -> i64;
    fn bank_name(&self) -> &str;
    fn detect(&self, text: &str) -> bool;
    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>>;
    /// Optional: parse directly from positioned text items extracted via lopdf.
    /// Return None to fall back to the text-based path.
    fn parse_positioned(
        &self,
        _items: &[lopdf_text::TextItem],
    ) -> Option<Result<Vec<ParsedTransaction>>> {
        None
    }
    /// Optional: detect from positioned items. Return None to fall back to text.
    fn detect_positioned(&self, _items: &[lopdf_text::TextItem]) -> Option<bool> {
        None
    }
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

/// Detect and parse using positioned text items (lopdf fallback path).
pub fn detect_and_parse_positioned(
    items: &[lopdf_text::TextItem],
) -> Result<(i64, String, Vec<ParsedTransaction>)> {
    for parser in all_parsers() {
        let detected = parser
            .detect_positioned(items)
            .unwrap_or_else(|| {
                // Fallback: build plain text from items and use text-based detect
                let text = lopdf_text::items_to_text(items, 5.0);
                parser.detect(&text)
            });

        if detected {
            tracing::info!(bank = %parser.bank_name(), "Parser matched via positioned items");
            if let Some(result) = parser.parse_positioned(items) {
                let transactions = result?;
                tracing::info!(
                    bank         = %parser.bank_name(),
                    transactions = %transactions.len(),
                    "Positioned parse complete"
                );
                return Ok((parser.bank_id(), parser.bank_name().to_string(), transactions));
            }
            // Parser detected but has no positioned implementation — fall back to text
            let text = lopdf_text::items_to_text(items, 5.0);
            let transactions = parser.parse(&text)?;
            return Ok((parser.bank_id(), parser.bank_name().to_string(), transactions));
        }
    }
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
