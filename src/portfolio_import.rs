//! Portfolio file import — turn an uploaded CSV / Excel / PDF into `HoldingInput`
//! rows for the Portfolio page.
//!
//! Display-only ingest of the user's OWN holdings; never places an order. Best-effort
//! and conservative: malformed rows are collected into `warnings`, never fabricated.
//! Excel/CSV are reliable (broker exports); PDF is best-effort text extraction (broker
//! PDFs vary wildly — the user is told to verify/edit the parsed rows).

use crate::holdings_analytics::{parse_csv, parse_text};
use crate::types::HoldingInput;

pub struct Import {
    pub holdings: Vec<HoldingInput>,
    pub warnings: Vec<String>,
    pub source: String,
}

/// Detect format from filename and parse into holdings rows.
pub fn import_bytes(filename: &str, bytes: &[u8]) -> Import {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".xlsx") || lower.ends_with(".xls") || lower.ends_with(".xlsm") || lower.ends_with(".ods") {
        return import_excel(bytes);
    }
    if lower.ends_with(".pdf") {
        return import_pdf(bytes);
    }
    // Default: CSV (then best-effort text).
    let (h, w) = parse_csv(bytes);
    if !h.is_empty() {
        return Import { holdings: h, warnings: w, source: "csv".into() };
    }
    let text = String::from_utf8_lossy(bytes);
    let (h2, mut w2) = parse_text(&text);
    if h2.is_empty() {
        w2.push("Could not read this file as CSV/text. Try an Excel (.xlsx) or CSV export.".into());
    }
    Import { holdings: h2, warnings: w2, source: "csv".into() }
}

fn cell_to_string(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.replace(',', " "),
        Data::Float(f) => format!("{f}"),
        Data::Int(i) => format!("{i}"),
        Data::Bool(b) => format!("{b}"),
        other => format!("{other}"),
    }
}

fn import_excel(bytes: &[u8]) -> Import {
    use calamine::{open_workbook_auto_from_rs, Reader};
    use std::io::Cursor;
    let mut warnings = Vec::new();
    let mut wb = match open_workbook_auto_from_rs(Cursor::new(bytes.to_vec())) {
        Ok(w) => w,
        Err(e) => {
            return Import {
                holdings: vec![],
                warnings: vec![format!("could not read the spreadsheet: {e}")],
                source: "excel".into(),
            };
        }
    };
    // First non-empty sheet → reconstruct a CSV string → reuse the header-aliasing
    // parser (so Zerodha/Groww/INDmoney column names are auto-detected).
    let names = wb.sheet_names().to_owned();
    let mut csv_text = String::new();
    for name in &names {
        if let Ok(range) = wb.worksheet_range(name) {
            if range.is_empty() {
                continue;
            }
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(cell_to_string).collect();
                csv_text.push_str(&cells.join(","));
                csv_text.push('\n');
            }
            break;
        }
    }
    if csv_text.trim().is_empty() {
        warnings.push("the spreadsheet had no readable rows".into());
        return Import { holdings: vec![], warnings, source: "excel".into() };
    }
    let (h, w) = parse_csv(csv_text.as_bytes());
    warnings.extend(w);
    if h.is_empty() {
        warnings.push("no holdings rows recognised — the sheet needs columns for symbol, quantity and average price".into());
    }
    Import { holdings: h, warnings, source: "excel".into() }
}

fn import_pdf(bytes: &[u8]) -> Import {
    let mut warnings = vec!["PDF parsing is best-effort — broker PDFs vary, so verify the rows below and edit if needed. For the most reliable import, upload an Excel (.xlsx) or CSV export from your broker.".to_string()];
    match pdf_extract::extract_text_from_mem(bytes) {
        Ok(text) => {
            let (h, w) = parse_text(&text);
            warnings.extend(w);
            if h.is_empty() {
                warnings.push("couldn't recognise any holdings rows in the PDF text — please upload an Excel/CSV export instead, or paste the rows.".into());
            }
            Import { holdings: h, warnings, source: "pdf".into() }
        }
        Err(e) => {
            warnings.push(format!("could not extract text from the PDF: {e}"));
            Import { holdings: vec![], warnings, source: "pdf".into() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_bytes_parse_via_aliasing() {
        let csv = b"tradingsymbol,quantity,average price\nRELIANCE,10,1300\n";
        let imp = import_bytes("holdings.csv", csv);
        assert_eq!(imp.holdings.len(), 1);
        assert_eq!(imp.holdings[0].symbol, "RELIANCE");
        assert_eq!(imp.source, "csv");
    }

    #[test]
    fn unknown_extension_falls_back_to_csv_then_text() {
        let imp = import_bytes("blob.dat", b"INFY 5 1500\n");
        // best-effort text parse picks up the triple
        assert_eq!(imp.holdings.len(), 1);
        assert_eq!(imp.holdings[0].symbol, "INFY");
    }

    #[test]
    fn empty_pdf_warns_not_panics() {
        let imp = import_bytes("statement.pdf", b"%PDF-1.4 not a real pdf");
        assert_eq!(imp.source, "pdf");
        assert!(!imp.warnings.is_empty());
        assert!(imp.holdings.is_empty());
    }
}
