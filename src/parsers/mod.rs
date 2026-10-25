pub mod bnp_paribas;
pub mod credit_mutuel;
pub mod lopdf_text;

use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

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
    /// Return true to use lopdf-positioned extraction as the primary parse path.
    /// When true, `safe_detect_and_parse` skips pdf_extract and calls parse_positioned
    /// directly, which allows debit/credit detection via column X coordinates.
    fn prefers_positioned(&self) -> bool {
        false
    }
    /// Extract (opening_balance, closing_balance) from the statement text.
    /// Returns None if the lines cannot be found — validation is skipped in that case.
    fn extract_balances(&self, text: &str) -> Option<(f64, f64)> {
        extract_statement_balances(text)
    }
}

/// Shared balance extractor — works for both BNP Paribas and Crédit Mutuel.
/// Finds lines like:
///   "SOLDE CREDITEUR AU 26.12.2025       3 930,65"
///   "SOLDE CREDITEUR AU 30/04/2026       11.171,17"
///   "Réf : 005 SOLDE CREDITEUR AU 01/06/2026       16.368,49"
/// Returns the first balance as the opening balance and the last as the closing balance.
pub fn extract_statement_balances(text: &str) -> Option<(f64, f64)> {
    let re = Regex::new(
        r"(?i)SOLDE\s+(CR[EÉ]DITEUR|D[EÉ]BITEUR)\s+AU\s+[\d./]+\s+([\d\s.\x{00A0}]+,\d{2})",
    )
    .ok()?;

    let mut balances: Vec<f64> = Vec::new();
    for cap in re.captures_iter(text) {
        let is_debit = cap[1].to_uppercase().contains("DEBITEUR");
        if let Some(amount) = parse_european_amount(cap[2].trim()) {
            balances.push(if is_debit { -amount } else { amount });
        }
    }

    if balances.len() >= 2 {
        tracing::debug!(
            opening = %balances[0],
            closing = %balances.last().unwrap(),
            "Balance lines found"
        );
        Some((balances[0], *balances.last().unwrap()))
    } else {
        tracing::debug!(found = balances.len(), "Not enough balance lines for validation");
        None
    }
}

/// Validate that the sum of parsed transaction amounts matches the
/// difference between the closing and opening balances.
/// Returns Err if validation fails; Ok if it passes or balances are not found.
fn validate_balance(
    bank: &str,
    transactions: &[ParsedTransaction],
    start_bal: f64,
    end_bal: f64,
) -> Result<()> {
    let tx_sum: f64 = transactions.iter().map(|t| t.amount).sum();
    let expected = end_bal - start_bal;
    let error = (tx_sum - expected).abs();

    tracing::info!(
        bank        = %bank,
        start_bal   = %start_bal,
        end_bal     = %end_bal,
        tx_sum      = %tx_sum,
        expected    = %expected,
        error       = %error,
        "Balance validation"
    );

    if error > 0.005 {
        anyhow::bail!(
            "Validation du solde échouée :\n\
             • Somme des transactions analysées : {:.2} €\n\
             • Variation de solde attendue (solde final − solde initial) : {:.2} €\n\
             • Écart : {:.2} €\n\n\
             Le fichier n'a pas été sauvegardé. Vérifiez que le relevé est complet et non tronqué.",
            tx_sum,
            expected,
            error
        );
    }
    Ok(())
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
            if let Some((start_bal, end_bal)) = parser.extract_balances(text) {
                validate_balance(parser.bank_name(), &transactions, start_bal, end_bal)?;
            }
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
                let text_for_validation = lopdf_text::items_to_text(items, 5.0);
                if let Some((start_bal, end_bal)) = parser.extract_balances(&text_for_validation) {
                    validate_balance(parser.bank_name(), &transactions, start_bal, end_bal)?;
                }
                return Ok((parser.bank_id(), parser.bank_name().to_string(), transactions));
            }
            // Parser detected but has no positioned implementation — fall back to text
            let text = lopdf_text::items_to_text(items, 5.0);
            let transactions = parser.parse(&text)?;
            if let Some((start_bal, end_bal)) = parser.extract_balances(&text) {
                validate_balance(parser.bank_name(), &transactions, start_bal, end_bal)?;
            }
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
