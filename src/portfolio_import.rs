//! Portfolio file import — turn an uploaded Excel / CSV (or pasted rows) into
//! `HoldingInput` rows for the Portfolio page.
//!
//! Display-only ingest of the user's OWN holdings; never places an order. Best-effort
//! and conservative: malformed rows are collected into `warnings`, never fabricated.
//! Excel/CSV are reliable (broker exports). The parser detects the header row, so a
//! broker statement with preamble rows above the table (name / client code / summary)
//! is read correctly. PDF import was removed — broker PDFs vary too wildly to parse
//! reliably; users export an Excel/CSV or paste the rows instead.

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
        return Import {
            holdings: Vec::new(),
            warnings: vec![
                "PDF import isn't supported — broker PDFs vary too much to parse reliably. Export an Excel (.xlsx) or CSV from your broker, or paste the rows below.".to_string(),
            ],
            source: "unsupported".into(),
        };
    }
    // Default: CSV/TSV (header auto-detected); then best-effort whitespace text.
    let (h, w) = parse_csv(bytes);
    if !h.is_empty() {
        return Import { holdings: h, warnings: w, source: "csv".into() };
    }
    let text = String::from_utf8_lossy(bytes);
    let (h2, mut w2) = parse_text(&text);
    if h2.is_empty() {
        w2.push("Could not read this file. Upload an Excel (.xlsx) or CSV export, or paste the rows.".into());
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
        warnings.push("no holdings rows recognised — the sheet needs columns for stock/symbol, quantity and average price".into());
    }
    Import { holdings: h, warnings, source: "excel".into() }
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
    fn pdf_is_unsupported_with_guidance() {
        let imp = import_bytes("statement.pdf", b"%PDF-1.4 not a real pdf");
        assert_eq!(imp.source, "unsupported");
        assert!(imp.warnings.iter().any(|w| w.to_lowercase().contains("pdf")));
        assert!(imp.holdings.is_empty());
    }
}
