use super::{lopdf_text::TextItem, parse_european_amount, BankParser};
use crate::models::ParsedTransaction;
use anyhow::Result;
use regex::Regex;

pub struct BnpParibasParser;

impl BankParser for BnpParibasParser {
    fn bank_id(&self) -> i64 { 1 }
    fn bank_name(&self) -> &str { "BNP Paribas" }

    fn prefers_positioned(&self) -> bool { true }

    fn detect(&self, text: &str) -> bool {
        text.contains("BNP PARIBAS") || text.contains("BNP Paribas") || text.contains("BNPPARIBAS")
    }

    /// Positioned parse using lopdf X coordinates to distinguish Débit / Crédit columns.
    /// This is the primary parse path (prefers_positioned = true).
    fn parse_positioned(&self, items: &[TextItem]) -> Option<Result<Vec<ParsedTransaction>>> {
        Some(parse_bnp_positioned(items))
    }

    /// Text-based fallback (used only when lopdf extraction fails).
    /// Sign is determined by operation keyword since pdf_extract collapses column spacing.
    fn parse(&self, text: &str) -> Result<Vec<ParsedTransaction>> {
        let (start_year, start_month, end_year, end_month) = extract_years(text);

        let line_re = Regex::new(
            r"^(\d{2})\.(\d{2})\s+(\d{2})\.(\d{2})\s+([0-9\x{00A0} ]+,[0-9]{2})\*?\s*(.*?)\s*$",
        )?;
        let skip_re = Regex::new(
            r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+des\s+op[eé]rations)",
        )?;
        let tx_start_re = Regex::new(r"^(\d{2})\.(\d{2})\s")?;

        let mut transactions: Vec<ParsedTransaction> = Vec::new();
        let mut pending: Option<(String, String, f64, Vec<String>, Option<String>)> = None;

        for line in text.lines() {
            if skip_re.is_match(line) {
                flush(&mut pending, &mut transactions);
                continue;
            }

            if let Some(cap) = line_re.captures(line) {
                flush(&mut pending, &mut transactions);

                let op_month: u32 = cap[2].parse().unwrap_or(0);
                let val_month: u32 = cap[4].parse().unwrap_or(0);
                let amount_str = cap[5].trim();
                let description = cap[6].trim().to_string();

                let op_year  = infer_year(op_month,  start_month, end_month, start_year, end_year);
                let val_year = infer_year(val_month, start_month, end_month, start_year, end_year);

                let date   = format!("{:04}-{:02}-{}", op_year,  op_month,  &cap[1]);
                let valeur = format!("{:04}-{:02}-{}", val_year, val_month, &cap[3]);

                if let Some(abs_amount) = parse_european_amount(amount_str) {
                    let amount = if is_credit_keyword(&description) { abs_amount } else { -abs_amount };
                    pending = Some((date, valeur, amount, vec![description], None));
                }
            } else if let Some(sc) = tx_start_re.captures(line) {
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
        tracing::debug!(count = transactions.len(), "BNP text-fallback parser finished");
        Ok(transactions)
    }
}

// ── Positioned parser ─────────────────────────────────────────────────────────

fn parse_bnp_positioned(items: &[TextItem]) -> Result<Vec<ParsedTransaction>> {
    let plain_text = super::lopdf_text::items_to_text(items, 5.0);
    let (start_year, start_month, end_year, end_month) = extract_years(&plain_text);

    // Detect the X threshold that separates the Débit column from the Crédit column.
    let credit_col_x = detect_credit_col_x(items);
    tracing::info!(credit_col_x = %credit_col_x, "BNP positioned: Débit/Crédit threshold");

    let date_re = Regex::new(r"^\d{2}\.\d{2}$").unwrap();
    // Matches items that START with an amount (digits + comma + 2 dec) optionally
    // followed immediately by description text (no space separator in some PDFs).
    let amount_re = Regex::new(r"^([\d\u{00A0} ]+,\d{2})\*?\s*(.*?)$").unwrap();
    let skip_re = Regex::new(
        r"(?i)(solde\s+cr[eé]diteur|solde\s+d[eé]biteur|total\s+des\s+op[eé]rations)",
    )
    .unwrap();

    let rows = rows_from_items(items);
    let mut transactions: Vec<ParsedTransaction> = Vec::new();
    let mut pending: Option<(String, String, f64, Vec<String>)> = None;

    for row in &rows {
        let row_text: String = row.iter().map(|i| i.text.as_str()).collect::<Vec<_>>().join(" ");
        if skip_re.is_match(&row_text) {
            flush_pos(&mut pending, &mut transactions);
            continue;
        }

        let mut dates: Vec<(f64, String)>          = Vec::new();
        let mut amount: Option<(f64, String)>       = None; // (x, raw_amount_str)
        let mut descs: Vec<(f64, String)>           = Vec::new();

        for item in row.iter() {
            let t = item.text.trim();
            if date_re.is_match(t) {
                dates.push((item.x, t.to_string()));
                continue;
            }
            if let Some(cap) = amount_re.captures(t) {
                let amt_str  = cap[1].trim().to_string();
                let desc_str = cap[2].trim().to_string();
                if parse_european_amount(&amt_str).is_some() {
                    // Keep the rightmost amount in the row (should be only one normally)
                    if amount.as_ref().map(|(x, _)| item.x > *x).unwrap_or(true) {
                        amount = Some((item.x, amt_str));
                    }
                    if !desc_str.is_empty() {
                        descs.push((item.x, desc_str));
                    }
                    continue;
                }
            }
            descs.push((item.x, t.to_string()));
        }

        if dates.len() >= 2 && amount.is_some() {
            flush_pos(&mut pending, &mut transactions);

            dates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

            let (op_day, op_month_s) = dates[0].1.split_once('.').unwrap_or(("01", "01"));
            let op_month: u32 = op_month_s.parse().unwrap_or(1);
            let op_year = infer_year(op_month, start_month, end_month, start_year, end_year);

            let (val_day, val_month_s) = dates[1].1.split_once('.').unwrap_or(("01", "01"));
            let val_month: u32 = val_month_s.parse().unwrap_or(1);
            let val_year = infer_year(val_month, start_month, end_month, start_year, end_year);

            let date   = format!("{:04}-{:02}-{}", op_year,  op_month,  op_day);
            let valeur = format!("{:04}-{:02}-{}", val_year, val_month, val_day);

            let (amt_x, amt_str) = amount.unwrap();
            let is_credit = amt_x >= credit_col_x;

            if let Some(abs) = parse_european_amount(&amt_str) {
                let signed = if is_credit { abs } else { -abs };
                descs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                let op = descs.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" ");
                pending = Some((date, valeur, signed, vec![op]));
            }
        } else if dates.is_empty() && amount.is_none() && !descs.is_empty() {
            // Continuation line — append to pending description
            if let Some((_, _, _, ref mut ops)) = pending {
                descs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                let cont = descs.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" ");
                let cont = cont.trim().to_string();
                if !cont.is_empty() {
                    ops.push(cont);
                }
            }
        }
    }

    flush_pos(&mut pending, &mut transactions);
    tracing::debug!(count = transactions.len(), credit_col_x = %credit_col_x, "BNP positioned parse done");
    Ok(transactions)
}

/// Find the X coordinate threshold between the Débit and Crédit column headers.
/// Returns the midpoint between "Débit" and "Crédit" header item X positions,
/// or a hard-coded fallback if the headers cannot be found.
///
/// Run `cargo test dump_positioned -- --ignored --nocapture` against a real BNP PDF
/// to inspect actual coordinates and verify/adjust FALLBACK_CREDIT_COL_X below.
fn detect_credit_col_x(items: &[TextItem]) -> f64 {
    const FALLBACK_CREDIT_COL_X: f64 = 440.0;

    for row in rows_from_items(items).iter() {
        let mut debit_x: Option<f64>  = None;
        let mut credit_x: Option<f64> = None;

        for item in row.iter() {
            let lower = item.text.trim().to_lowercase();
            // Handle both accented (é = U+00E9) and unaccented variants
            if lower == "débit" || lower == "debit" {
                debit_x = Some(item.x);
            }
            if lower == "crédit" || lower == "credit" {
                credit_x = Some(item.x);
            }
        }

        if let (Some(dx), Some(cx)) = (debit_x, credit_x) {
            let threshold = (dx + cx) / 2.0;
            tracing::debug!(
                debit_x    = %dx,
                credit_x   = %cx,
                threshold  = %threshold,
                "BNP: Débit/Crédit column headers found"
            );
            return threshold;
        }
    }

    tracing::warn!(
        fallback = %FALLBACK_CREDIT_COL_X,
        "BNP: Débit/Crédit headers not found in positioned items, using fallback threshold. \
         Run dump_positioned test to calibrate."
    );
    FALLBACK_CREDIT_COL_X
}

/// Group positioned text items into rows by Y coordinate proximity (within 3 pt).
fn rows_from_items(items: &[TextItem]) -> Vec<Vec<&TextItem>> {
    let mut sorted: Vec<&TextItem> = items.iter().collect();
    sorted.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then(b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut rows: Vec<Vec<&TextItem>> = Vec::new();
    let mut current: Vec<&TextItem>   = Vec::new();
    let mut cur_y    = f64::MAX;
    let mut cur_page = usize::MAX;

    for item in sorted {
        if (item.y - cur_y).abs() > 3.0 || item.page != cur_page {
            if !current.is_empty() {
                rows.push(current);
                current = Vec::new();
            }
            cur_y    = item.y;
            cur_page = item.page;
        }
        current.push(item);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn flush_pos(
    pending: &mut Option<(String, String, f64, Vec<String>)>,
    out: &mut Vec<ParsedTransaction>,
) {
    if let Some((d, v, a, ops)) = pending.take() {
        out.push(ParsedTransaction {
            date: d,
            transaction_date: v,
            operation: ops.join(" "),
            amount: a,
            parse_note: None,
        });
    }
}

// ── Text-fallback helpers ─────────────────────────────────────────────────────

/// Keyword-based credit detection used only in the text-fallback parse path.
fn is_credit_keyword(description: &str) -> bool {
    let desc = description.to_uppercase();
    desc.starts_with("REMBOURST")
        || desc.starts_with("REMBOURSEMENT")
        || desc.starts_with("AVOIR")
        || desc.starts_with("VIR RECU")
        || desc.starts_with("VIR.RECU")
        || desc.starts_with("VIREMENT RECU")
        || desc.contains("INTERETS CREDIT")
        || desc.starts_with("REMISE CHEQUE")
        || desc.starts_with("REM. CHQ")
        || desc.starts_with("REMISE CHQ")
        || desc.starts_with("CREDIT DE SALAIRE")
}

fn flush(
    pending: &mut Option<(String, String, f64, Vec<String>, Option<String>)>,
    out: &mut Vec<ParsedTransaction>,
) {
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

// ── Shared helpers ────────────────────────────────────────────────────────────

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
    else if s.starts_with('f') { 2 }
    else if s.starts_with("mar") { 3 }
    else if s.starts_with("avr") { 4 }
    else if s.starts_with("mai") { 5 }
    else if s.starts_with("juil") { 7 }
    else if s.starts_with("juin") { 6 }
    else if s.starts_with("ao") { 8 }
    else if s.starts_with("sep") { 9 }
    else if s.starts_with("oct") { 10 }
    else if s.starts_with("nov") { 11 }
    else if s.starts_with('d') { 12 }
    else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::BankParser;

    #[test]
    fn detects_bnp() {
        let p = BnpParibasParser;
        assert!(p.detect("BNP PARIBAS relevé de compte"));
        assert!(!p.detect("Crédit Mutuel"));
    }

    #[test]
    fn year_inference() {
        assert_eq!(infer_year(12, 12, 1, 2025, 2026), 2025);
        assert_eq!(infer_year(1,  12, 1, 2025, 2026), 2026);
        assert_eq!(infer_year(3,   1, 12, 2025, 2025), 2025);
    }

    /// Verify that the text-fallback parser still works for the known sample.
    /// The positioned parser is the primary path and tested via dump_positioned.
    #[test]
    fn text_fallback_parses_sample() {
        const SAMPLE: &str = "\
RELEVE DE COMPTE CHEQUES
du 26 décembre 2025 au 26 janvier 2026

SOLDE CREDITEUR AU 26.12.2025 3 930,65
26.12 29.12 43,38REMBOURST CB DU 271225 GREENGO
00 SAINT-OUEN CARTE 4974XXXXXXXX6008
26.12 29.12 43,38FACTURE(S) CARTE 4974XXXXXXXX6008
DU 251225 GREENGO 00 SAINT-OUEN
26.12 29.12 129,88REMBOURST CB DU 271225 GREENGO
00 SAINT-OUEN CARTE 4974XXXXXXXX6008
26.12 29.12 149,60DU 261225 GREENGO 00 SAINT-OUEN
29.12 29.12 584,40DU 271225 ACS PARIS
05.01 05.01 6,87* COMMISSIONS COTISATION A UNE OFFRE
GROUPEE DE SERVICES ESPRIT LIBRE
16.01 16.01 80,00RETRAIT CARTE ESPECES
16/01/26 16H39 061736
BNP GRENOBLE 086001108900
26.01 26.01 6,02DU 230126 CITIZ ALPES LOI
FRA 6,02EUR
TOTAL DES OPERATIONS 1 416,07 173,26
SOLDE CREDITEUR AU 26.01.2026 2 687,84";

        let p = BnpParibasParser;
        let txs = p.parse(SAMPLE).expect("text fallback parse");
        assert_eq!(txs.len(), 8);

        assert!((txs[0].amount - 43.38).abs() < 0.01, "REMBOURST → credit");
        assert!((txs[1].amount + 43.38).abs() < 0.01, "FACTURE → debit");
        assert!((txs[2].amount - 129.88).abs() < 0.01, "second REMBOURST → credit");
        assert_eq!(txs[5].date, "2026-01-05");
        assert_eq!(txs[6].date, "2026-01-16");
        assert!((txs[6].amount + 80.00).abs() < 0.01);
        assert!((txs[7].amount + 6.02).abs() < 0.01);
    }

    /// Print raw text extracted by pdf_extract.
    /// cargo test dump_raw_text -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_raw_text() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/BNP_Paribs_2026-01-01.pdf");
        let text = pdf_extract::extract_text(path).expect("extract");
        for (i, line) in text.lines().enumerate() {
            println!("{:04}: {:?}", i, line);
        }
    }

    /// Print X/Y coordinates for every text item.
    /// Use this to calibrate FALLBACK_CREDIT_COL_X and verify detect_credit_col_x.
    /// cargo test dump_positioned -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_positioned() {
        use crate::parsers::lopdf_text;
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/BNP_Paribs_2026-01-01.pdf");
        let bytes = std::fs::read(path).expect("read");
        let items = lopdf_text::extract_positioned(&bytes).expect("extract");
        for item in &items {
            println!("p={} x={:7.2} y={:7.2}  {:?}", item.page, item.x, item.y, item.text);
        }
    }
}
