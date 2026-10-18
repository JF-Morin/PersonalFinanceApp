use super::{parse_european_amount, BankParser};
use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

pub struct CreditMutuelParser;

// Table columns: date | Date Valeur | Opération | Débit Euros | Crédit Euros
// Special: the 4-digit sequence in col 3 = "Date" (DD MM); if absent use Date Valeur.
// Lines to skip: "Solde créditeur au …"
impl BankParser for CreditMutuelParser {
    fn bank_id(&self) -> i64 { 2 }
    fn bank_name(&self) -> &str { "Crédit Mutuel" }

    fn detect(&self, text: &str) -> bool {
        text.contains("CREDIT MUTUEL")
            || text.contains("Crédit Mutuel")
            || text.contains("CAISSE CREDIT MUTUEL")
    }

    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>> {
        // Row pattern: DD/MM [DD/MM] operation [debit] [credit]
        // Date column is DD/MM (no year); year is inferred from context or current year.
        let row_re = Regex::new(
            r"(?m)^(\d{2}/\d{2})\s+(\d{2}/\d{2})\s+(.+?)\s+([\d\s]+,\d{2})?\s*([\d\s]+,\d{2})?\s*$",
        )?;

        // 4-digit sequence embedded in operation text (DD MM)
        let date_in_op_re = Regex::new(r"\b(\d{2})(\d{2})\b")?;

        let skip_re = Regex::new(
            r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|ancien\s+solde|nouveau\s+solde)",
        )?;

        // Lines where both date columns are empty (continuation of previous operation)
        let continuation_re = Regex::new(r"(?m)^\s{10,}(.+?)\s*$")?;

        let mut transactions: Vec<ParsedTransaction> = Vec::new();
        let current_year = chrono::Local::now().format("%Y").to_string();

        for cap in row_re.captures_iter(text) {
            let line = cap.get(0).unwrap().as_str();
            if skip_re.is_match(line) {
                continue;
            }

            let transaction_date_raw = &cap[1]; // col 1 → transaction_date
            let valeur_raw = &cap[2];           // col 2 → Date Valeur (fallback for date)
            let operation_raw = cap[3].trim();
            let debit_str = cap.get(4).map(|m| m.as_str().trim()).unwrap_or("");
            let credit_str = cap.get(5).map(|m| m.as_str().trim()).unwrap_or("");

            // Try to extract date from embedded 4-digit sequence in operation
            let (date, operation) = if let Some(m) = date_in_op_re.find(operation_raw) {
                let seq = m.as_str();
                let dd = &seq[0..2];
                let mm = &seq[2..4];
                let d = format!("{}-{}-{}", current_year, mm, dd);
                let op = operation_raw.replacen(seq, "", 1).trim().to_string();
                (d, op)
            } else {
                (convert_date(valeur_raw, &current_year), operation_raw.to_string())
            };

            let transaction_date = convert_date(transaction_date_raw, &current_year);

            let amount = if !debit_str.is_empty() {
                parse_european_amount(debit_str).map(|v| -v)
            } else if !credit_str.is_empty() {
                parse_european_amount(credit_str)
            } else {
                None
            };

            if let Some(amt) = amount {
                transactions.push(ParsedTransaction {
                    date,
                    transaction_date,
                    operation,
                    amount: amt,
                });
            }
        }

        // Append continuation lines to previous operation
        // (simplified: just ignore them for the first draft)
        let _ = continuation_re;

        Ok(transactions)
    }
}

/// Convert DD/MM → YYYY-MM-DD using the provided year.
fn convert_date(s: &str, year: &str) -> String {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2 {
        format!("{}-{}-{}", year, parts[1], parts[0])
    } else {
        s.to_string()
    }
}
