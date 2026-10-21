use super::{lopdf_text::TextItem, parse_european_amount, BankParser};
use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

pub struct CreditMutuelParser;

impl BankParser for CreditMutuelParser {
    fn bank_id(&self) -> i64 { 2 }
    fn bank_name(&self) -> &str { "Crédit Mutuel" }

    fn detect(&self, text: &str) -> bool {
        text.contains("CREDIT MUTUEL")
            || text.contains("Crédit Mutuel")
            || text.contains("CAISSE CREDIT MUTUEL")
    }

    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>> {
        // Transaction opening line: DD/MM/YYYY DD/MM/YYYY <op><spaces><amount>
        //
        // (.+\S) = operation text ending with a non-space char; prevents the
        // greedy match from consuming the column-spacing before the amount.
        // (\s+) = whitespace between operation and amount; length distinguishes
        // the Débit column (few spaces) from the Crédit column (many spaces).
        // Amounts use either "," decimal ("99,42") or "." thousands + "," decimal ("3.000,00").
        let line_re = Regex::new(
            r"^(\d{2}/\d{2}/\d{4})\s+(\d{2}/\d{2}/\d{4})\s+(.+\S)(\s+)([0-9.\x{00A0} ]+,[0-9]{2})\s*$",
        )?;
        let skip_re = Regex::new(
            r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+mensuel|total\s+des\s+frais|r[eé]f\s*:)",
        )?;
        let tx_start_re = Regex::new(r"^(\d{2}/\d{2}/\d{4})\s")?;

        let mut transactions: Vec<ParsedTransaction> = Vec::new();
        let mut pending: Option<(String, String, f64, Vec<String>, Option<String>)> = None;

        for line in text.lines() {
            if skip_re.is_match(line) {
                flush(&mut pending, &mut transactions);
                continue;
            }

            if let Some(cap) = line_re.captures(line) {
                flush(&mut pending, &mut transactions);

                let date = convert_date(&cap[1]);
                let valeur = convert_date(&cap[2]);
                let op_text = cap[3].trim().to_string();
                let spacing_len = cap[4].len();
                let amount_str = cap[5].trim();

                if let Some(abs_amount) = parse_european_amount(amount_str) {
                    // Credit-column amounts are placed further right in the PDF, so
                    // pdf_extract inserts more leading spaces. Threshold: ≥ 6 spaces = credit.
                    // Run `dump_raw_text` (see tests below) to inspect actual spacing and
                    // calibrate if all transactions appear as debits.
                    let amount = if spacing_len >= 6 { abs_amount } else { -abs_amount };
                    pending = Some((date, valeur, amount, vec![op_text], None));
                }
            } else if let Some(sc) = tx_start_re.captures(line) {
                // Date-prefixed line that didn't match the full transaction regex → broken.
                flush(&mut pending, &mut transactions);
                let date = convert_date(&sc[1]);
                let raw = line.trim().to_string();
                tracing::warn!(line = %raw, bank = "Crédit Mutuel", "Ligne non analysée");
                transactions.push(ParsedTransaction {
                    date: date.clone(),
                    transaction_date: date,
                    operation: raw.clone(),
                    amount: 0.0,
                    parse_note: Some(format!("Ligne non analysée : {}", raw)),
                });
            } else if let Some((_, _, _, ref mut ops, _)) = pending {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    ops.push(trimmed.to_string());
                }
            }
        }
        flush(&mut pending, &mut transactions);

        tracing::debug!(count = transactions.len(), "Crédit Mutuel parser finished");
        Ok(transactions)
    }

    fn detect_positioned(&self, items: &[TextItem]) -> Option<bool> {
        Some(items.iter().any(|i| {
            i.text.contains("CREDIT MUTUEL")
                || i.text.contains("Crédit Mutuel")
                || i.text.contains("CAISSE CREDIT MUTUEL")
        }))
    }

    fn parse_positioned(&self, items: &[TextItem]) -> Option<Result<Vec<ParsedTransaction>>> {
        Some(parse_from_positioned(items))
    }
}

/// Parse Credit Mutuel transactions directly from positioned text items (lopdf fallback).
///
/// Column layout from PDF analysis (in points):
///   Date1:       X ≈  52 – 99
///   Date2:       X ≈ 100 – 147
///   Operation:   X ≈ 148 – 429
///   Debit:       X ≈ 430 – 489
///   Credit:      X ≥ 490
fn parse_from_positioned(items: &[TextItem]) -> Result<Vec<ParsedTransaction>> {
    let date_re = Regex::new(r"^\d{2}/\d{2}/\d{4}$")?;
    // Matches any header/footer text that must never be treated as a transaction
    // or continuation line.
    let skip_re = Regex::new(
        r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+mensuel|total\s+des\s+frais|\
r[eé]f\s*:|opération|d[eé]bit\s+euros|cr[eé]dit\s+euros|sous\s+r[eé]serve|libell[eé]|\
iban\s*:|page\s+\d|ccm\s+grenoble|avenue\s+alsace|creditmutuel|bic\s*:)",
    )?;

    // Sort by page first, then top-to-bottom (Y descending within page), then left-to-right.
    // This is critical: without the page key, items from different pages at the same Y
    // coordinate get merged into the same row.
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then(b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    // Group into rows: new row when Y changes by more than 3 pt OR page changes.
    let mut rows: Vec<Vec<&TextItem>> = Vec::new();
    let mut current_y = f64::MAX;
    let mut current_page = usize::MAX;

    for item in &sorted {
        if (item.y - current_y).abs() > 3.0 || item.page != current_page {
            rows.push(Vec::new());
            current_y = item.y;
            current_page = item.page;
        }
        rows.last_mut().unwrap().push(item);
    }

    let mut transactions: Vec<ParsedTransaction> = Vec::new();
    let mut pending: Option<(String, String, f64, Vec<String>, Option<String>)> = None;

    for row in &rows {
        let mut cols: Vec<(&f64, &str)> = row.iter().map(|i| (&i.x, i.text.as_str())).collect();
        cols.sort_by(|a, b| a.0.partial_cmp(b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Skip rows that are entirely header/footer text
        let full_row = cols.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(" ");
        if skip_re.is_match(&full_row) {
            flush(&mut pending, &mut transactions);
            continue;
        }

        let dates: Vec<&&f64> = cols
            .iter()
            .filter(|(x, t)| **x < 150.0 && date_re.is_match(t))
            .map(|(x, _)| x)
            .collect();

        if dates.len() >= 2 {
            flush(&mut pending, &mut transactions);

            let date1 = cols.iter().find(|(x, t)| **x < 100.0 && date_re.is_match(t)).map(|(_, t)| *t);
            let date2 = cols.iter().find(|(x, t)| **x >= 100.0 && **x < 150.0 && date_re.is_match(t)).map(|(_, t)| *t);

            let op_parts: Vec<&str> = cols
                .iter()
                .filter(|(x, t)| **x >= 140.0 && **x < 430.0 && !skip_re.is_match(t))
                .map(|(_, t)| *t)
                .collect();
            let op_text = op_parts.join(" ");

            let debit: Option<f64> = cols
                .iter()
                .filter(|(x, _)| **x >= 430.0 && **x < 490.0)
                .filter_map(|(_, t)| parse_european_amount(t))
                .next();
            let credit: Option<f64> = cols
                .iter()
                .filter(|(x, _)| **x >= 490.0)
                .filter_map(|(_, t)| parse_european_amount(t))
                .next();

            if let (Some(d1), Some(d2)) = (date1, date2) {
                let date = convert_date(d1);
                let valeur = convert_date(d2);

                match (debit, credit) {
                    (Some(a), _) => {
                        pending = Some((date, valeur, -a, vec![op_text], None));
                    }
                    (None, Some(a)) => {
                        pending = Some((date, valeur, a, vec![op_text], None));
                    }
                    (None, None) => {
                        let raw = cols.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(" ");
                        transactions.push(ParsedTransaction {
                            date: date.clone(),
                            transaction_date: date,
                            operation: raw.clone(),
                            amount: 0.0,
                            parse_note: Some(format!("Montant introuvable : {}", raw)),
                        });
                    }
                }
            }
        } else if let Some((_, _, _, ref mut ops, _)) = pending {
            // Continuation line: only text in the operation column range
            let continuation: Vec<&str> = cols
                .iter()
                .filter(|(x, t)| **x >= 140.0 && **x < 430.0 && !skip_re.is_match(t))
                .map(|(_, t)| *t)
                .collect();
            let s = continuation.join(" ");
            let s = s.trim();
            if !s.is_empty() {
                ops.push(s.to_string());
            }
        }
    }

    flush(&mut pending, &mut transactions);
    tracing::debug!(count = transactions.len(), "Crédit Mutuel positioned parser finished");
    Ok(transactions)
}

fn flush(pending: &mut Option<(String, String, f64, Vec<String>, Option<String>)>, out: &mut Vec<ParsedTransaction>) {
    if let Some((d, v, a, ops, note)) = pending.take() {
        out.push(ParsedTransaction {
            date: d,
            transaction_date: v,
            operation: ops.join(" "),
            amount: a,
            parse_note: note,
        });
    }
}

/// DD/MM/YYYY → YYYY-MM-DD
fn convert_date(s: &str) -> String {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 {
        format!("{}-{}-{}", p[2], p[1], p[0])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::BankParser;

    // Sample mirroring the expected pdf_extract output.
    // Credits have 7 spaces before the amount (credit column position);
    // debits have 1 space. Adjust the threshold in parse() if needed —
    // use `dump_raw_text` to inspect actual spacing from the real PDF.
    const SAMPLE: &str = "\
RELEVE ET INFORMATIONS BANCAIRES
SOLDE CREDITEUR AU 30/04/2026       11.171,17
02/05/2026 02/05/2026 VIR SEPA LOYER MAI 2026 980,00
02/05/2026 02/05/2026 VIR SEPA MATOS SPELEO (DESCENDE 141,00
04/05/2026 04/05/2026 PAIEMENT PSC 0205 GRENOBLE 39,95
AU VIEUX CAMPEUR CARTE 4212
07/05/2026 01/05/2026 F COTIS EUROCOMPTE ESSENTIEL 9,40
10/05/2026 10/05/2026 VIR INST JEAN-FRANCOIS MOIN       3.000,00
REMBOURSEMENT PARTIEL 1 DE 2 POUR 2025 (3000 DE
14/05/2026 14/05/2026 PAIEMENT CB 1205 493518627463 99,42
MOL*EXTREMTEXTIL CARTE 4212
28/05/2026 28/05/2026 VIR AMETEK SAS       3.013,15
Réf : 005 SOLDE CREDITEUR AU 01/06/2026       16.368,49";

    #[test]
    fn detects_credit_mutuel() {
        let p = CreditMutuelParser;
        assert!(p.detect("CAISSE DE CREDIT MUTUEL GRENOBLE"));
        assert!(p.detect("Crédit Mutuel statement"));
        assert!(!p.detect("BNP PARIBAS"));
    }

    #[test]
    fn parses_sample() {
        let p = CreditMutuelParser;
        let txs = p.parse(SAMPLE).expect("parse");

        assert_eq!(txs.len(), 7, "expected 7 transactions in sample");

        // Simple debit
        assert_eq!(txs[0].date, "2026-05-02");
        assert_eq!(txs[0].transaction_date, "2026-05-02");
        assert_eq!(txs[0].operation, "VIR SEPA LOYER MAI 2026");
        assert!((txs[0].amount + 980.00).abs() < 0.01);

        // Debit with continuation merchant line
        assert_eq!(txs[2].date, "2026-05-04");
        assert!(txs[2].operation.contains("PAIEMENT PSC 0205 GRENOBLE"));
        assert!(txs[2].operation.contains("AU VIEUX CAMPEUR CARTE 4212"), "continuation appended");
        assert!((txs[2].amount + 39.95).abs() < 0.01);

        // Debit with different valeur date
        assert_eq!(txs[3].date, "2026-05-07");
        assert_eq!(txs[3].transaction_date, "2026-05-01");
        assert!((txs[3].amount + 9.40).abs() < 0.01);

        // Credit with dot-thousands separator and continuation
        assert_eq!(txs[4].date, "2026-05-10");
        assert!(txs[4].operation.contains("VIR INST JEAN-FRANCOIS MOIN"));
        assert!(txs[4].operation.contains("REMBOURSEMENT PARTIEL 1"), "continuation appended");
        assert!((txs[4].amount - 3000.00).abs() < 0.01, "credit should be positive");

        // Debit with long card number in operation
        assert!(txs[5].operation.contains("493518627463"));
        assert!(txs[5].operation.contains("MOL*EXTREMTEXTIL CARTE 4212"), "continuation appended");
        assert!((txs[5].amount + 99.42).abs() < 0.01);

        // Salary credit
        assert_eq!(txs[6].date, "2026-05-28");
        assert_eq!(txs[6].operation, "VIR AMETEK SAS");
        assert!((txs[6].amount - 3013.15).abs() < 0.01, "credit should be positive");
    }

    #[test]
    fn convert_date_roundtrip() {
        assert_eq!(convert_date("02/05/2026"), "2026-05-02");
        assert_eq!(convert_date("01/06/2026"), "2026-06-01");
    }

    /// Prints raw text extracted from the real PDF to inspect column spacing.
    /// Run with: cargo test dump_raw_text -- --nocapture
    #[test]
    #[ignore]
    fn dump_raw_text() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/Credit_Mutuel_2026-06-01.pdf");
        let text = pdf_extract::extract_text(path).expect("extract");
        for (i, line) in text.lines().enumerate() {
            println!("{:04}: {:?}", i, line);
        }
    }
}
