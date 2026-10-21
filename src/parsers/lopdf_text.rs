/// Positioned text extraction using lopdf.
/// Returns (page, x, y, text) for each non-rotated text span.
/// Used as a fallback when pdf_extract panics (e.g. Type3 fonts).
use anyhow::Result;
use lopdf::Object;

#[derive(Debug, Clone)]
pub struct TextItem {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub text: String,
}

pub fn extract_positioned(bytes: &[u8]) -> Result<Vec<TextItem>> {
    let doc = lopdf::Document::load_mem(bytes)?;
    let mut items = Vec::new();

    for (page_num, page_id) in doc.page_iter().enumerate() {
        let content = match doc.get_and_decode_page_content(page_id) {
            Ok(c) => c,
            Err(_) => continue,
        };
        extract_from_content(&content, page_num, &mut items);
    }

    Ok(items)
}

/// Concatenate all extracted text into a single string, grouping items by Y row.
/// Items are sorted top-to-bottom (decreasing Y) then left-to-right (increasing X).
/// Spaces between items in the same row are inserted proportional to the X gap
/// divided by `char_width_pt`.
pub fn items_to_text(items: &[TextItem], char_width_pt: f64) -> String {
    if items.is_empty() {
        return String::new();
    }

    // Group into rows by Y (within 3 pt)
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then(b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut lines: Vec<String> = Vec::new();
    let mut current_y = f64::MAX;
    let mut current_page = usize::MAX;
    let mut row: Vec<(f64, String)> = Vec::new();

    for item in &sorted {
        if (item.y - current_y).abs() > 3.0 || item.page != current_page {
            if !row.is_empty() {
                lines.push(render_row(&row, char_width_pt));
                row.clear();
            }
            current_y = item.y;
            current_page = item.page;
        }
        row.push((item.x, item.text.clone()));
    }
    if !row.is_empty() {
        lines.push(render_row(&row, char_width_pt));
    }

    lines.join("\n")
}

fn render_row(row: &[(f64, String)], char_width_pt: f64) -> String {
    let mut result = String::new();
    let mut prev_end_x = f64::MIN;
    let mut first = true;

    for (x, text) in row {
        if first {
            first = false;
        } else {
            let gap = x - prev_end_x;
            let spaces = ((gap / char_width_pt).round() as usize).max(1);
            for _ in 0..spaces {
                result.push(' ');
            }
        }
        result.push_str(text);
        // Approximate end position: start + text length * char_width
        prev_end_x = x + text.len() as f64 * char_width_pt;
    }

    result
}

fn extract_from_content(
    content: &lopdf::content::Content,
    page_num: usize,
    items: &mut Vec<TextItem>,
) {
    let mut in_text = false;
    // Text matrix: [a, b, c, d, x, y]
    let mut tm = [1.0_f64, 0.0, 0.0, 1.0, 0.0, 0.0];
    // Line matrix (updated by Td/TD, copied to tm)
    let mut lm = [1.0_f64, 0.0, 0.0, 1.0, 0.0, 0.0];

    for op in &content.operations {
        match op.operator.as_str() {
            "BT" => {
                in_text = true;
                tm = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                lm = tm;
            }
            "ET" => {
                in_text = false;
            }
            "Tm" if in_text && op.operands.len() == 6 => {
                for (i, v) in op.operands.iter().enumerate() {
                    tm[i] = obj_f64(v);
                }
                lm = tm;
            }
            "Td" | "TD" if in_text && op.operands.len() == 2 => {
                lm[4] += obj_f64(&op.operands[0]);
                lm[5] += obj_f64(&op.operands[1]);
                tm = lm;
            }
            "T*" if in_text => {
                // Move to next line — we don't track TL so just leave position
            }
            "Tj" if in_text => {
                if let Some(Object::String(ref b, _)) = op.operands.first() {
                    push_item(items, page_num, &tm, decode_bytes(b));
                }
            }
            "TJ" if in_text => {
                if let Some(Object::Array(ref arr)) = op.operands.first() {
                    let mut s = String::new();
                    for obj in arr {
                        match obj {
                            Object::String(ref b, _) => s.push_str(&decode_bytes(b)),
                            Object::Integer(n) if *n < -200 => s.push(' '),
                            _ => {}
                        }
                    }
                    push_item(items, page_num, &tm, s);
                }
            }
            _ => {}
        }
    }
}

fn push_item(items: &mut Vec<TextItem>, page: usize, tm: &[f64; 6], text: String) {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    // Skip rotated text (b != 0 means rotation component present)
    if tm[1].abs() > 0.01 || tm[2].abs() > 0.01 {
        return;
    }
    items.push(TextItem {
        page,
        x: tm[4],
        y: tm[5],
        text: trimmed,
    });
}

fn decode_bytes(bytes: &[u8]) -> String {
    // UTF-16 BE BOM
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let words: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| {
                if c.len() == 2 {
                    ((c[0] as u16) << 8) | (c[1] as u16)
                } else {
                    c[0] as u16
                }
            })
            .collect();
        return String::from_utf16_lossy(&words).to_string();
    }
    // Latin-1 / WinAnsiEncoding
    bytes
        .iter()
        .filter(|&&b| b >= 32 || b == b'\n')
        .map(|&b| b as char)
        .collect()
}

fn obj_f64(obj: &Object) -> f64 {
    match obj {
        Object::Real(f) => *f as f64,
        Object::Integer(i) => *i as f64,
        _ => 0.0,
    }
}
