use super::{parse_european_amount, BankParser};
use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

pub struct BnpParibasParser;

// Table columns: date | Nature des opérations | Valeur | Débit | Crédit
// Lines to skip: "Solde créditeur au …", "Total des opérations"
impl BankParser for BnpParibasParser {
    fn bank_id(&self) -> i64 { 1 }
    fn bank_name(&self) -> &str { "BNP Paribas" }

    fn detect(&self, text: &str) -> bool {
        text.contains("BNP PARIBAS") || text.contains("BNP Paribas") || text.contains("BNPPARIBAS")
    }

    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>> {
        // Row pattern: DD/MM/YYYY <operation text> DD/MM/YYYY [debit] [credit]
        let row_re = Regex::new(
            r"(?m)^(\d{2}/\d{2}/\d{4})\s+(.+?)\s+(\d{2}/\d{2}/\d{4})\s+([\d\s]+,\d{2})?\s*([\d\s]+,\d{2})?\s*$",
        )?;

        let skip_re = Regex::new(
            r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+des\s+op[eé]rations|ancien\s+solde|nouveau\s+solde)",
        )?;

        let mut transactions = Vec::new();

        for cap in row_re.captures_iter(text) {
            let line = cap.get(0).unwrap().as_str();
            if skip_re.is_match(line) {
                continue;
            }

            let date_raw = cap[1].trim();
            let operation = cap[2].trim().to_string();
            let valeur_raw = cap[3].trim();
            let debit_str = cap.get(4).map(|m| m.as_str().trim()).unwrap_or("");
            let credit_str = cap.get(5).map(|m| m.as_str().trim()).unwrap_or("");

            let date = convert_date(date_raw);
            let transaction_date = convert_date(valeur_raw);

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

        Ok(transactions)
    }
}

/// Convert DD/MM/YYYY → YYYY-MM-DD
fn convert_date(s: &str) -> String {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 3 {
        format!("{}-{}-{}", parts[2], parts[1], parts[0])
    } else {
        s.to_string()
    }
}
