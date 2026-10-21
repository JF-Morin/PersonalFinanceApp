use super::{parse_european_amount, BankParser};
use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

pub struct BnpParibasParser;

impl BankParser for BnpParibasParser {
    fn bank_id(&self) -> i64 { 1 }
    fn bank_name(&self) -> &str { "BNP Paribas" }

    fn detect(&self, text: &str) -> bool {
        text.contains("BNP PARIBAS") || text.contains("BNP Paribas") || text.contains("BNPPARIBAS")
    }

    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>> {
        let (start_year, start_month, end_year, end_month) = extract_years(text);

        // Transaction opening line: DD.MM <op text> DD.MM<spaces><amount>
        // Group 1/2: day/month of operation date
        // Group 3: operation text (greedy, stops before last DD.MM + amount)
        // Group 4/5: day/month of valeur date
        // Group 6: spaces between valeur and amount (length → debit vs credit column)
        // Group 7: amount string
        let line_re = Regex::new(
            r"^(\d{2})\.(\d{2})\s+(.+)\s+(\d{2})\.(\d{2})(\s+)([0-9\x{00A0} ]+,[0-9]{2})\s*$",
        )?;
        let skip_re = Regex::new(
            r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+des\s+op[eé]rations)",
        )?;
        let tx_start_re = Regex::new(r"^(\d{2})\.(\d{2})\s")?;

        let mut transactions: Vec<ParsedTransaction> = Vec::new();
        // Accumulator: (date, valeur, amount, op_parts, parse_note)
        let mut pending: Option<(String, String, f64, Vec<String>, Option<String>)> = None;

        for line in text.lines() {
            if skip_re.is_match(line) {
                flush(&mut pending, &mut transactions);
                continue;
            }

            if let Some(cap) = line_re.captures(line) {
                flush(&mut pending, &mut transactions);

                let op_month: u32 = cap[2].parse().unwrap_or(0);
                let val_month: u32 = cap[5].parse().unwrap_or(0);
                let spacing_len = cap[6].len();
                let amount_str = cap[7].trim();

                let op_year = infer_year(op_month, start_month, end_month, start_year, end_year);
                let val_year = infer_year(val_month, start_month, end_month, start_year, end_year);

                let date = format!("{:04}-{:02}-{}", op_year, op_month, &cap[1]);
                let valeur = format!("{:04}-{:02}-{}", val_year, val_month, &cap[4]);

                if let Some(abs_amount) = parse_european_amount(amount_str) {
                    // Amounts in the Crédit column are placed further right in the PDF, so
                    // pdf_extract inserts more leading spaces. Threshold: ≥ 6 spaces = credit.
                    // If all transactions appear as debits, calibrate this value using the
                    // `dump_raw_text` test below to inspect the actual whitespace.
                    let amount = if spacing_len >= 6 { abs_amount } else { -abs_amount };
                    pending = Some((date, valeur, amount, vec![cap[3].trim().to_string()], None));
                }
            } else if let Some(sc) = tx_start_re.captures(line) {
                // Date-prefixed line that didn't match the full transaction regex → broken.
                flush(&mut pending, &mut transactions);
                let op_month: u32 = sc[2].parse().unwrap_or(0);
                let year = infer_year(op_month, start_month, end_month, start_year, end_year);
                let date = format!("{:04}-{:02}-{}", year, op_month, &sc[1]);
                let raw = line.trim().to_string();
                tracing::warn!(line = %raw, bank = "BNP Paribas", "Ligne non analysée");
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

        tracing::debug!(count = transactions.len(), "BNP parser finished");
        Ok(transactions)
    }
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

/// Extract (start_year, start_month, end_year, end_month) from
/// "du DD mois YYYY au DD mois YYYY" header line.
fn extract_years(text: &str) -> (i32, u32, i32, u32) {
    let re = Regex::new(
        r"(?i)du\s+\d+\s+([^\s]+)\s+(\d{4})\s+au\s+\d+\s+([^\s]+)\s+(\d{4})",
    )
    .unwrap();
    if let Some(cap) = re.captures(text) {
        let sm = french_month(&cap[1]);
        let sy: i32 = cap[2].parse().unwrap_or(2025);
        let em = french_month(&cap[3]);
        let ey: i32 = cap[4].parse().unwrap_or(sy);
        return (sy, sm, ey, em);
    }
    (2025, 1, 2025, 12)
}

/// For cross-year statements (e.g. Dec 2025 – Jan 2026):
/// months ≤ end_month belong to end_year; others belong to start_year.
fn infer_year(month: u32, _start_month: u32, end_month: u32, start_year: i32, end_year: i32) -> i32 {
    if start_year == end_year {
        return start_year;
    }
    if month <= end_month { end_year } else { start_year }
}

fn french_month(s: &str) -> u32 {
    let s = s.to_lowercase();
    let s = s.trim_matches(|c: char| !c.is_alphabetic());
    if s.starts_with("jan") { 1 }
    else if s.starts_with('f') { 2 }       // février
    else if s.starts_with("mar") { 3 }
    else if s.starts_with("avr") { 4 }
    else if s.starts_with("mai") { 5 }
    else if s.starts_with("juil") { 7 }
    else if s.starts_with("juin") { 6 }
    else if s.starts_with("ao") { 8 }      // août
    else if s.starts_with("sep") { 9 }
    else if s.starts_with("oct") { 10 }
    else if s.starts_with("nov") { 11 }
    else if s.starts_with('d') { 12 }      // décembre
    else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::BankParser;

    // Sample that mirrors the expected pdf_extract output format.
    // Credits (REMBOURST) have 7 spaces before the amount (credit column);
    // debits have 1 space. Adjust CREDIT_THRESHOLD in parse() if the actual
    // pdf_extract output uses different spacing — use `dump_raw_text` to verify.
    const SAMPLE: &str = "\
RELEVE DE COMPTE CHEQUES
du 26 décembre 2025 au 26 janvier 2026

SOLDE CREDITEUR AU 26.12.2025       3 930,65
26.12 REMBOURST CB DU 271225 GREENGO 29.12       43,38
00 SAINT-OUEN CARTE 4974XXXXXXXX6008
26.12 FACTURE(S) CARTE 4974XXXXXXXX6008 29.12 43,38
DU 251225 GREENGO 00 SAINT-OUEN
26.12 DU 261225 GREENGO 00 SAINT-OUEN 29.12 149,60
29.12 DU 271225 ACS PARIS 29.12 584,40
05.01 *COMMISSIONS COTISATION A UNE OFFRE 05.01 6,87
GROUPEE DE SERVICES ESPRIT LIBRE
16.01 RETRAIT CARTE ESPECES 16.01 80,00
16/01/26 16H39 061736
BNP GRENOBLE 086001108900
26.01 DU 230126 CITIZ ALPES LOI 26.01 6,02
FRA 6,02EUR
TOTAL DES OPERATIONS 1 416,07 173,26
SOLDE CREDITEUR AU 26.01.2026       2 687,84";

    #[test]
    fn detects_bnp() {
        let p = BnpParibasParser;
        assert!(p.detect("BNP PARIBAS relevé de compte"));
        assert!(!p.detect("Crédit Mutuel"));
    }

    #[test]
    fn parses_sample() {
        let p = BnpParibasParser;
        let txs = p.parse(SAMPLE).expect("parse");

        assert_eq!(txs.len(), 7, "expected 7 transactions in sample");

        // First: credit (7 spaces before amount → positive)
        assert_eq!(txs[0].date, "2025-12-26");
        assert_eq!(txs[0].transaction_date, "2025-12-29");
        assert!(txs[0].operation.contains("REMBOURST"));
        assert!(txs[0].operation.contains("00 SAINT-OUEN"), "continuation should be appended");
        assert!((txs[0].amount - 43.38).abs() < 0.01, "REMBOURST should be credit (+)");

        // Second: debit (1 space → negative)
        assert_eq!(txs[1].date, "2025-12-26");
        assert!(txs[1].operation.contains("FACTURE(S)"));
        assert!(txs[1].operation.contains("DU 251225"), "continuation should be appended");
        assert!((txs[1].amount + 43.38).abs() < 0.01, "FACTURE should be debit (-)");

        // Year crossing: January → 2026
        assert_eq!(txs[4].date, "2026-01-05");
        assert!(txs[4].operation.contains("GROUPEE DE SERVICES"), "continuation appended");

        // Three-line RETRAIT
        assert_eq!(txs[5].date, "2026-01-16");
        assert!(txs[5].operation.contains("RETRAIT CARTE ESPECES"));
        assert!(txs[5].operation.contains("BNP GRENOBLE"), "second continuation appended");
        assert!((txs[5].amount + 80.00).abs() < 0.01);

        // CITIZ with EUR continuation
        assert_eq!(txs[6].date, "2026-01-26");
        assert!(txs[6].operation.contains("CITIZ ALPES LOI"));
        assert!((txs[6].amount + 6.02).abs() < 0.01);
    }

    #[test]
    fn year_inference() {
        // Dec → 2025, Jan → 2026 for a cross-year statement
        assert_eq!(infer_year(12, 12, 1, 2025, 2026), 2025);
        assert_eq!(infer_year(1, 12, 1, 2025, 2026), 2026);
        // Same-year statement
        assert_eq!(infer_year(3, 1, 12, 2025, 2025), 2025);
    }

    /// Prints the raw text extracted from the real PDF so you can inspect
    /// column spacing and calibrate the credit-column threshold.
    /// Run with: cargo test dump_raw_text -- --nocapture
    #[test]
    #[ignore]
    fn dump_raw_text() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/BNP_Paribs_2026-01-01.pdf");
        let text = pdf_extract::extract_text(path).expect("extract");
        for (i, line) in text.lines().enumerate() {
            println!("{:04}: {:?}", i, line);
        }
    }
}
