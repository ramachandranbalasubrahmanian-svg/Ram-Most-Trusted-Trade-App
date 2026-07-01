//! Trade-journal import — turn a pasted table or an uploaded broker report
//! (Excel/CSV: a realized-P&L / tradewise statement, or rows with buy+sell prices)
//! into journal rows the user can review and analyse.
//!
//! RECORD-ONLY & FIREWALLED. This module imports only `types` (+ the `csv` and
//! `calamine` crates). It NEVER touches signals, Confidence, scoring, ranking, the
//! edge map, sizing, or an anchor. It only converts the user's OWN realized trades
//! into `JournalEntry` rows; the (already display-only) Calibration Scorecard and
//! Portfolio Analytics read those back. Best-effort and conservative: unreadable
//! rows go to `warnings`, never fabricated. Nothing here ever places an order.

use crate::types::{JournalEntry, SignalState};

/// A parsed trade row, before it becomes a `JournalEntry`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TradeRow {
    pub symbol: String,
    /// Normalised "BUY" | "SELL".
    pub direction: String,
    pub qty: i64,
    pub entry_price: Option<f64>,
    pub exit_price: Option<f64>,
    /// Realized P&L if the statement supplied it directly (already signed).
    pub pnl: Option<f64>,
    /// Best-effort "YYYY-MM-DD".
    pub date: Option<String>,
    pub strategy: Option<String>,
    /// Distinct entry/exit execution timestamps when the statement carries them
    /// (raw string as parsed). Falls back to `date` for both when absent — which is
    /// why an imported realized-P&L statement with no times shows entry == exit.
    pub entry_time: Option<String>,
    pub exit_time: Option<String>,
    /// Intended / order (limit) price when present, so true slippage vs the fill can
    /// be computed. Realized-P&L statements rarely carry it, so slippage stays None.
    pub order_price: Option<f64>,
}

pub struct TradeImport {
    pub trades: Vec<TradeRow>,
    pub warnings: Vec<String>,
    pub source: String,
}

/// Detect format from the filename and parse into trade rows.
pub fn import_trades_bytes(filename: &str, bytes: &[u8]) -> TradeImport {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".xlsx") || lower.ends_with(".xls") || lower.ends_with(".xlsm") || lower.ends_with(".ods") {
        return import_excel(bytes);
    }
    if lower.ends_with(".pdf") {
        return TradeImport {
            trades: Vec::new(),
            warnings: vec![
                "PDF import isn't supported — broker PDFs vary too much to parse reliably. Export an Excel (.xlsx) or CSV of your tradebook / realized-P&L statement, or paste the rows below.".to_string(),
            ],
            source: "unsupported".into(),
        };
    }
    let (t, w) = parse_trades_csv(bytes);
    TradeImport { trades: t, warnings: w, source: "csv".into() }
}

fn cell_to_string(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.replace(',', " "),
        Data::Float(f) => format!("{f}"),
        Data::Int(i) => format!("{i}"),
        Data::Bool(b) => format!("{b}"),
        Data::DateTime(d) => format!("{}", d.as_f64()),
        other => format!("{other}"),
    }
}

fn import_excel(bytes: &[u8]) -> TradeImport {
    use calamine::{open_workbook_auto_from_rs, Reader};
    use std::io::Cursor;
    let mut warnings = Vec::new();
    let mut wb = match open_workbook_auto_from_rs(Cursor::new(bytes.to_vec())) {
        Ok(w) => w,
        Err(e) => {
            return TradeImport {
                trades: vec![],
                warnings: vec![format!("could not read the spreadsheet: {e}")],
                source: "excel".into(),
            };
        }
    };
    // First non-empty sheet → reconstruct CSV text → reuse the header-aliasing parser
    // (so Zerodha / Groww / Console column names are auto-detected).
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
        return TradeImport { trades: vec![], warnings, source: "excel".into() };
    }
    let (t, w) = parse_trades_csv(csv_text.as_bytes());
    warnings.extend(w);
    if t.is_empty() {
        warnings.push("no trade rows recognised — the sheet needs a symbol column plus either a realized-P&L column, or buy+sell prices.".into());
    }
    TradeImport { trades: t, warnings, source: "excel".into() }
}

// ---------------------------------------------------------------------------
// CSV / paste parsing with broker-agnostic header aliasing + header detection.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct ColMap {
    sym: Option<usize>,
    side: Option<usize>,
    qty: Option<usize>,
    entry: Option<usize>,
    exit: Option<usize>,
    pnl: Option<usize>,
    date: Option<usize>,
    strategy: Option<usize>,
    entry_time: Option<usize>,
    exit_time: Option<usize>,
    order_price: Option<usize>,
}

impl ColMap {
    /// How many trade-relevant columns this header resolved.
    fn resolved(&self) -> usize {
        [
            self.sym, self.side, self.qty, self.entry, self.exit, self.pnl, self.date,
            self.strategy, self.entry_time, self.exit_time, self.order_price,
        ]
        .iter()
        .filter(|o| o.is_some())
        .count()
    }
    /// A usable header has a symbol AND at least one outcome column (pnl, or both
    /// legs / enough to imply one).
    fn usable(&self) -> bool {
        self.sym.is_some() && (self.pnl.is_some() || self.entry.is_some() || self.exit.is_some())
    }
}

/// Lowercase + keep only ASCII alphanumerics, so "Realized P&L", "realised_pnl"
/// and "Net PnL " all collapse toward a comparable token.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn header_map(rec: &csv::StringRecord) -> Option<ColMap> {
    let mut cm = ColMap::default();
    for (i, raw) in rec.iter().enumerate() {
        let n = norm(raw);
        if n.is_empty() {
            continue;
        }
        let is_pnl = n.contains("pnl") || n.contains("profit") || n.contains("realiz") || n.contains("realis")
            || n == "pl" || n == "pandl" || n == "netpl" || n == "netpnl";
        // Time columns must be classified BEFORE the price columns so "entry time" /
        // "buy time" don't get mistaken for a buy PRICE.
        let is_time = n.contains("time") && !n.contains("runtime");
        let is_entry_time = is_time && (n.contains("entry") || n.contains("buy"));
        let is_exit_time = is_time && (n.contains("exit") || n.contains("sell"));
        let is_order_price = !is_pnl
            && (n == "orderprice" || n == "limitprice" || n == "intendedprice"
                || (n.contains("order") && n.contains("price")) || (n.contains("limit") && n.contains("price")));
        let is_entry = !is_pnl && !is_time && !is_order_price
            && (n.contains("entry") || n == "buyprice" || n == "buyavg" || n == "avgbuy" || n == "avgbuyprice"
                || n == "buyaverage" || n == "buyrate" || (n.contains("buy") && (n.contains("price") || n.contains("avg") || n.contains("rate"))));
        let is_exit = !is_pnl && !is_time && !is_order_price
            && (n.contains("exit") || n == "sellprice" || n == "sellavg" || n == "avgsell" || n == "avgsellprice"
                || n == "sellaverage" || n == "sellrate" || (n.contains("sell") && (n.contains("price") || n.contains("avg") || n.contains("rate"))));
        let is_qty = n == "qty" || n.contains("quantity") || n == "shares" || n == "filledqty" || n == "tradedqty" || n == "qtytraded";
        let is_symbol = n.contains("symbol") || n == "tradingsymbol" || n == "scrip" || n == "scripname"
            || n == "stock" || n == "stockname" || n == "instrument" || n == "ticker" || n == "name";
        let is_date = n.contains("date") || is_time;
        let is_strategy = n.contains("strateg") || n.contains("setup") || n.contains("remark") || n == "tag" || n.contains("note");

        // Assign each role to the FIRST matching column only. Specific columns
        // (order price, entry/exit time) are claimed before their generic cousins
        // (price, date) so a richer statement maps its extra columns correctly.
        if is_symbol && cm.sym.is_none() { cm.sym = Some(i); continue; }
        if is_pnl && cm.pnl.is_none() { cm.pnl = Some(i); continue; }
        if is_order_price && cm.order_price.is_none() { cm.order_price = Some(i); continue; }
        if is_entry_time && cm.entry_time.is_none() { cm.entry_time = Some(i); continue; }
        if is_exit_time && cm.exit_time.is_none() { cm.exit_time = Some(i); continue; }
        if is_entry && cm.entry.is_none() { cm.entry = Some(i); continue; }
        if is_exit && cm.exit.is_none() { cm.exit = Some(i); continue; }
        if is_qty && cm.qty.is_none() { cm.qty = Some(i); continue; }
        if is_side(&n) && cm.side.is_none() { cm.side = Some(i); continue; }
        if is_date && cm.date.is_none() { cm.date = Some(i); continue; }
        if is_strategy && cm.strategy.is_none() { cm.strategy = Some(i); continue; }
    }
    if cm.usable() { Some(cm) } else { None }
}

fn is_side(n: &str) -> bool {
    n == "side" || n == "direction" || n == "type" || n == "tradetype" || n == "buysell"
        || n == "bs" || n == "transactiontype" || n == "ordertype" || n == "action"
        || n.contains("buysell") || n.contains("tradetype")
}

/// Normalise a side cell to "BUY" | "SELL". Defaults to BUY when ambiguous (a long
/// round-trip is the common case; the P&L itself carries the sign).
fn normalize_side(raw: &str) -> String {
    let n = norm(raw);
    if n.starts_with('s') || n.contains("sell") || n.contains("short") {
        "SELL".to_string()
    } else {
        "BUY".to_string()
    }
}

/// Best-effort number parse: strips currency symbols, commas, spaces, and wraps
/// "(1,234.50)" accounting-negatives. Returns None for blank/non-numeric.
fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let neg = (t.starts_with('(') && t.ends_with(')')) || t.contains('-');
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if cleaned.is_empty() || cleaned == "." {
        return None;
    }
    cleaned.parse::<f64>().ok().map(|v| if neg { -v.abs() } else { v })
}

/// Normalise a date cell toward "YYYY-MM-DD" (best effort). Handles ISO,
/// dd-mm-yyyy, dd/mm/yyyy, and timestamps with a trailing time.
fn normalize_date(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let head = t.split_whitespace().next().unwrap_or(t);
    // ISO already: 2026-06-28...
    if head.len() >= 10 && &head[4..5] == "-" {
        return Some(head[..10].to_string());
    }
    // dd-mm-yyyy or dd/mm/yyyy
    let parts: Vec<&str> = head.split(|c| c == '-' || c == '/' || c == '.').collect();
    if parts.len() == 3 {
        let (a, b, c) = (parts[0], parts[1], parts[2]);
        if a.len() == 4 {
            return Some(format!("{:0>4}-{:0>2}-{:0>2}", a, b, c));
        }
        if c.len() == 4 {
            return Some(format!("{:0>4}-{:0>2}-{:0>2}", c, b, a));
        }
    }
    Some(head.to_string())
}

fn sniff_delimiter(bytes: &[u8]) -> u8 {
    let sample = &bytes[..bytes.len().min(8192)];
    let (mut tab, mut comma, mut semi) = (0usize, 0usize, 0usize);
    for &b in sample {
        match b {
            b'\t' => tab += 1,
            b',' => comma += 1,
            b';' => semi += 1,
            _ => {}
        }
    }
    if tab >= comma && tab >= semi && tab > 0 {
        b'\t'
    } else if semi > comma && semi > 0 {
        b';'
    } else {
        b','
    }
}

/// Parse a trade table (CSV/TSV bytes), header auto-detected with aliasing. Falls
/// back to a documented positional paste when no header is found.
pub fn parse_trades_csv(bytes: &[u8]) -> (Vec<TradeRow>, Vec<String>) {
    let delim = sniff_delimiter(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .delimiter(delim)
        .from_reader(bytes);
    let records: Vec<csv::StringRecord> = rdr.records().filter_map(|r| r.ok()).collect();

    let mut out = Vec::new();
    let mut warnings = Vec::new();

    // Locate the header row: among header-shaped records prefer the one resolving
    // the MOST columns (the detailed table, not a sparse summary above it).
    let header_at = records
        .iter()
        .enumerate()
        .filter_map(|(i, rec)| header_map(rec).map(|cm| (i, cm)))
        .fold(None::<(usize, ColMap)>, |best, (i, cm)| match best {
            Some((_, ref b)) if b.resolved() >= cm.resolved() => best,
            _ => Some((i, cm)),
        });

    match header_at {
        Some((hidx, cm)) => {
            for (i, rec) in records.iter().enumerate().skip(hidx + 1) {
                if rec.iter().all(|f| f.trim().is_empty()) {
                    continue;
                }
                match parse_row(rec, &cm, i + 1) {
                    Ok(t) => out.push(t),
                    Err(Some(w)) => warnings.push(w),
                    Err(None) => {}
                }
            }
        }
        None => {
            // Positional fallback. Accepted shapes (per the documented paste format):
            //   symbol, side, qty, entry, exit
            //   symbol, side, pnl
            //   symbol, pnl
            let first = records.iter().find(|r| !r.iter().all(|f| f.trim().is_empty()));
            let looks_like_data = first.map_or(false, |r| {
                r.len() >= 2 && !norm(r.get(0).unwrap_or("")).is_empty()
                    && r.iter().skip(1).any(|c| parse_num(c).is_some())
            });
            if !looks_like_data {
                warnings.push(
                    "No recognised header. Paste columns as: symbol, side, qty, entry, exit — or symbol, side, P&L — or symbol, P&L.".to_string(),
                );
            } else {
                for (i, rec) in records.iter().enumerate() {
                    if rec.iter().all(|f| f.trim().is_empty()) {
                        continue;
                    }
                    match parse_positional(rec, i + 1) {
                        Ok(t) => out.push(t),
                        Err(Some(w)) => warnings.push(w),
                        Err(None) => {}
                    }
                }
            }
        }
    }

    if out.is_empty() && warnings.is_empty() {
        warnings.push("no trade rows recognised — needs a symbol column plus a realized-P&L column or buy+sell prices.".to_string());
    }
    (out, warnings)
}

fn get<'a>(rec: &'a csv::StringRecord, idx: Option<usize>) -> &'a str {
    idx.and_then(|i| rec.get(i)).unwrap_or("").trim()
}

fn parse_row(rec: &csv::StringRecord, cm: &ColMap, rownum: usize) -> Result<TradeRow, Option<String>> {
    let symbol = get(rec, cm.sym).to_uppercase();
    if symbol.is_empty() {
        return Err(None);
    }
    let direction = normalize_side(get(rec, cm.side));
    let qty = parse_num(get(rec, cm.qty)).map(|q| q.abs().round() as i64).unwrap_or(0);
    let entry_price = parse_num(get(rec, cm.entry)).filter(|p| *p > 0.0);
    let exit_price = parse_num(get(rec, cm.exit)).filter(|p| *p > 0.0);
    let pnl = parse_num(get(rec, cm.pnl));
    let date = normalize_date(get(rec, cm.date));
    let strategy = {
        let s = get(rec, cm.strategy);
        if s.is_empty() { None } else { Some(s.to_string()) }
    };
    let opt_str = |idx: Option<usize>| {
        let s = get(rec, idx);
        if s.is_empty() { None } else { Some(s.to_string()) }
    };
    let entry_time = opt_str(cm.entry_time);
    let exit_time = opt_str(cm.exit_time);
    let order_price = parse_num(get(rec, cm.order_price)).filter(|p| *p > 0.0);

    if pnl.is_none() && !(entry_price.is_some() && exit_price.is_some()) {
        return Err(Some(format!(
            "row {rownum}: '{symbol}' has no P&L and not both buy+sell prices — skipped"
        )));
    }
    Ok(TradeRow {
        symbol, direction, qty, entry_price, exit_price, pnl, date, strategy,
        entry_time, exit_time, order_price,
    })
}

fn parse_positional(rec: &csv::StringRecord, rownum: usize) -> Result<TradeRow, Option<String>> {
    let cells: Vec<&str> = rec.iter().map(|c| c.trim()).collect();
    let symbol = cells.first().copied().unwrap_or("").to_uppercase();
    if symbol.is_empty() {
        return Err(None);
    }
    // symbol, side, qty, entry, exit
    if cells.len() >= 5 {
        let side = normalize_side(cells[1]);
        let qty = parse_num(cells[2]).map(|q| q.abs().round() as i64).unwrap_or(0);
        let entry = parse_num(cells[3]).filter(|p| *p > 0.0);
        let exit = parse_num(cells[4]).filter(|p| *p > 0.0);
        if entry.is_some() && exit.is_some() {
            return Ok(TradeRow { symbol, direction: side, qty, entry_price: entry, exit_price: exit, pnl: None, ..Default::default() });
        }
    }
    // symbol, side, pnl
    if cells.len() >= 3 {
        if let Some(pnl) = parse_num(cells[2]) {
            let side = normalize_side(cells[1]);
            return Ok(TradeRow { symbol, direction: side, qty: 0, pnl: Some(pnl), ..Default::default() });
        }
    }
    // symbol, pnl
    if cells.len() >= 2 {
        if let Some(pnl) = parse_num(cells[1]) {
            return Ok(TradeRow { symbol, direction: "BUY".into(), qty: 0, pnl: Some(pnl), ..Default::default() });
        }
    }
    Err(Some(format!("row {rownum}: '{symbol}' — couldn't read a P&L or buy+sell prices; skipped")))
}

/// Convert a parsed trade into a closed `JournalEntry`. P&L is taken as-supplied,
/// else computed `qty·(exit−entry)·dir` (dir +1 BUY, −1 SELL). Returns an error
/// string if neither P&L nor both legs are present (can't realize an outcome).
pub fn to_journal_entry(t: &TradeRow, now_ist: &str) -> Result<JournalEntry, String> {
    let dir = if t.direction == "SELL" { -1.0 } else { 1.0 };
    let pnl = match t.pnl {
        Some(p) => p,
        None => match (t.entry_price, t.exit_price) {
            (Some(e), Some(x)) if t.qty > 0 => t.qty as f64 * (x - e) * dir,
            _ => return Err(format!("{}: no P&L and not enough price/qty to compute one", t.symbol)),
        },
    };
    let stamp = |d: &Option<String>| -> String {
        match d {
            Some(s) if s.len() >= 10 => format!("{} 00:00:00", &s[..10]),
            Some(s) if !s.is_empty() => s.clone(),
            _ => now_ist.to_string(),
        }
    };
    let base_ts = stamp(&t.date);
    // Prefer a real execution timestamp when the statement supplied one. A time-only
    // value is prefixed with the trade date; a full datetime is used as-is. Falls
    // back to the date-derived base stamp (why a timeless import shows entry == exit).
    let resolve_ts = |time: &Option<String>| -> String {
        match time {
            Some(raw) if !raw.trim().is_empty() => {
                let s = raw.trim();
                if s.len() >= 16 && (s.contains(' ') || s.contains('T')) {
                    s.replace('T', " ")
                } else if s.len() >= 10 && s.contains('-') {
                    s.to_string()
                } else if let Some(d) = t.date.as_ref().filter(|d| d.len() >= 10) {
                    format!("{} {}", &d[..10], s)
                } else {
                    s.to_string()
                }
            }
            _ => base_ts.clone(),
        }
    };
    let entry_ts = resolve_ts(&t.entry_time);
    let exit_ts = resolve_ts(&t.exit_time);
    // True slippage (fill − intended, direction-signed) only when the statement
    // carried a distinct intended/order price alongside the executed fill.
    let slippage = match (t.order_price, t.entry_price) {
        (Some(intended), Some(fill)) => Some((fill - intended) * dir),
        _ => None,
    };
    Ok(JournalEntry {
        id: 0,
        generated_ist: base_ts,
        entry_ist: Some(entry_ts),
        exit_ist: Some(exit_ts),
        instrument_token: 0,
        symbol: t.symbol.clone(),
        direction: t.direction.clone(),
        strategy: t.strategy.clone().unwrap_or_else(|| "Imported".to_string()),
        alpha_trigger: "imported trade".to_string(),
        intended_price: t.order_price.or(t.entry_price).or(t.exit_price).unwrap_or(0.0),
        actual_fill_price: t.entry_price,
        exit_price: t.exit_price,
        qty: t.qty,
        state: SignalState::ManuallyAccepted.as_str().to_string(),
        pnl: Some(pnl),
        slippage,
        sector: None,
    })
}

/// Stable key for deduplicating a re-uploaded report: same symbol, side, qty,
/// rounded P&L, and date ⇒ treated as the same trade.
pub fn dedup_key(e: &JournalEntry) -> String {
    let pnl = e.pnl.map(|p| (p * 100.0).round() as i64).unwrap_or(0);
    let date = e.exit_ist.as_deref().unwrap_or("").chars().take(10).collect::<String>();
    format!("{}|{}|{}|{}|{}", e.symbol, e.direction, e.qty, pnl, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zerodha_style_realized_pnl_headers() {
        let csv = b"Symbol,Quantity,Buy Average,Sell Average,Realized P&L\nRELIANCE,10,1300,1325,250\nINFY,5,1500,1480,-100\n";
        let (rows, w) = parse_trades_csv(csv);
        assert_eq!(rows.len(), 2, "warnings: {w:?}");
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert_eq!(rows[0].pnl, Some(250.0));
        assert_eq!(rows[0].entry_price, Some(1300.0));
        assert_eq!(rows[1].pnl, Some(-100.0));
    }

    #[test]
    fn computes_pnl_from_legs_when_absent() {
        let csv = b"tradingsymbol,trade type,qty,buy price,sell price\nTCS,BUY,3,3000,3100\n";
        let (rows, _) = parse_trades_csv(csv);
        assert_eq!(rows.len(), 1);
        let e = to_journal_entry(&rows[0], "2026-06-29 10:00:00").unwrap();
        // long 3 @ (3100-3000) = 300
        assert!((e.pnl.unwrap() - 300.0).abs() < 1e-9, "pnl={:?}", e.pnl);
        assert_eq!(e.state, "Manually_Accepted");
    }

    #[test]
    fn short_trade_pnl_is_direction_signed() {
        // Sold high, covered low → profit on a SELL.
        let t = TradeRow { symbol: "SBIN".into(), direction: "SELL".into(), qty: 10, entry_price: Some(600.0), exit_price: Some(580.0), pnl: None, ..Default::default() };
        let e = to_journal_entry(&t, "2026-06-29 10:00:00").unwrap();
        // 10 * (580-600) * -1 = +200
        assert!((e.pnl.unwrap() - 200.0).abs() < 1e-9, "pnl={:?}", e.pnl);
    }

    #[test]
    fn positional_symbol_pnl_paste() {
        let (rows, w) = parse_trades_csv(b"WIPRO, 450\nMARUTI, -120\n");
        assert_eq!(rows.len(), 2, "warnings: {w:?}");
        assert_eq!(rows[0].symbol, "WIPRO");
        assert_eq!(rows[0].pnl, Some(450.0));
        assert_eq!(rows[1].pnl, Some(-120.0));
    }

    #[test]
    fn positional_full_trade_paste() {
        let (rows, _) = parse_trades_csv(b"HDFCBANK, buy, 4, 1500, 1540\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry_price, Some(1500.0));
        assert_eq!(rows[0].exit_price, Some(1540.0));
        assert_eq!(rows[0].qty, 4);
    }

    #[test]
    fn accounting_negative_and_currency_are_parsed() {
        assert_eq!(parse_num("(1,234.50)"), Some(-1234.50));
        assert_eq!(parse_num("₹ 2,000"), Some(2000.0));
        assert_eq!(parse_num("-300"), Some(-300.0));
        assert_eq!(parse_num(""), None);
        assert_eq!(parse_num("abc"), None);
    }

    #[test]
    fn date_normalisation_handles_common_formats() {
        assert_eq!(normalize_date("2026-06-28 09:20:00").as_deref(), Some("2026-06-28"));
        assert_eq!(normalize_date("28-06-2026").as_deref(), Some("2026-06-28"));
        assert_eq!(normalize_date("28/06/2026").as_deref(), Some("2026-06-28"));
    }

    #[test]
    fn dedup_key_is_stable_for_same_trade() {
        let t = TradeRow { symbol: "INFY".into(), direction: "BUY".into(), qty: 5, pnl: Some(250.0), date: Some("2026-06-28".into()), ..Default::default() };
        let a = to_journal_entry(&t, "now").unwrap();
        let b = to_journal_entry(&t, "later").unwrap();
        assert_eq!(dedup_key(&a), dedup_key(&b));
    }

    #[test]
    fn imports_distinct_entry_exit_times() {
        // A statement carrying separate buy/sell execution times → distinct stamps.
        let csv = b"symbol,side,qty,buy price,sell price,entry time,exit time\nTCS,BUY,3,3000,3100,2026-06-30 09:45:00,2026-06-30 14:10:00\n";
        let (rows, w) = parse_trades_csv(csv);
        assert_eq!(rows.len(), 1, "warnings: {w:?}");
        assert_eq!(rows[0].entry_time.as_deref(), Some("2026-06-30 09:45:00"));
        assert_eq!(rows[0].exit_time.as_deref(), Some("2026-06-30 14:10:00"));
        // Buy/sell PRICE columns must NOT be stolen by the time detection.
        assert_eq!(rows[0].entry_price, Some(3000.0));
        assert_eq!(rows[0].exit_price, Some(3100.0));
        let e = to_journal_entry(&rows[0], "2026-06-30 21:00:00").unwrap();
        assert_eq!(e.entry_ist.as_deref(), Some("2026-06-30 09:45:00"));
        assert_eq!(e.exit_ist.as_deref(), Some("2026-06-30 14:10:00"));
        assert_ne!(e.entry_ist, e.exit_ist);
    }

    #[test]
    fn imports_slippage_when_order_price_present() {
        // Order (intended) 100.00 vs executed buy fill 100.50 on a BUY → +0.50 slip.
        let csv = b"symbol,side,qty,order price,buy price,sell price\nWIPRO,BUY,10,100.00,100.50,105.00\n";
        let (rows, w) = parse_trades_csv(csv);
        assert_eq!(rows.len(), 1, "warnings: {w:?}");
        assert_eq!(rows[0].order_price, Some(100.00));
        assert_eq!(rows[0].entry_price, Some(100.50));
        let e = to_journal_entry(&rows[0], "now").unwrap();
        assert!((e.slippage.unwrap() - 0.50).abs() < 1e-9, "slippage={:?}", e.slippage);
        assert!((e.intended_price - 100.00).abs() < 1e-9);
    }

    #[test]
    fn timeless_import_keeps_entry_equal_exit() {
        // No time columns → entry and exit stamps both fall back to the trade date.
        let (rows, _) = parse_trades_csv(b"symbol,pnl,date\nSBIN,500,2026-06-30\n");
        let e = to_journal_entry(&rows[0], "2026-06-30 21:00:00").unwrap();
        assert_eq!(e.entry_ist, e.exit_ist);
        assert!(e.slippage.is_none());
    }

    #[test]
    fn row_without_outcome_is_skipped_with_warning() {
        let (rows, w) = parse_trades_csv(b"Symbol,Buy Average,Sell Average\nFOO,,\n");
        assert!(rows.is_empty());
        assert!(w.iter().any(|m| m.contains("FOO") || m.to_lowercase().contains("no")), "warnings: {w:?}");
    }
}
