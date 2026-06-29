//! Company-name / ISIN → NSE trading-symbol resolver.
//!
//! Broker holdings statements identify a stock by its full company name (and ISIN),
//! not its NSE trading symbol — e.g. "HSG & URBAN DEV CORPN LTD" is `HUDCO`,
//! "TATA MOTORS LIMITED" is `TMCV` (the post-demerger commercial-vehicle entity).
//! This module loads the archive's `symbol_metadata.parquet` (symbol ↔ name ↔
//! longName ↔ sector) and resolves a raw statement name to the trading symbol the
//! rest of the platform uses.
//!
//! Order of resolution: (1) the raw token is ALREADY a known trading symbol →
//! pass through (broker CSVs that export symbols); (2) exact normalized-name match
//! (abbreviation-expanded); (3) token-set fuzzy match above a threshold; (4) no
//! match → keep the raw name and flag it, so a row is never silently mis-attributed.
//!
//! Read-only + display-only: this only maps names; it never places an order and
//! imports nothing from scoring/confidence.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use duckdb::Connection;

/// The outcome of resolving one raw name.
pub struct Resolved {
    pub symbol: String,
    pub sector: Option<String>,
    pub matched: bool,
    pub how: &'static str, // "symbol" | "name" | "fuzzy" | "unmatched"
}

/// Expand a single statement-shorthand token to its metadata long form.
fn expand(tok: &str) -> &str {
    match tok {
        "LTD" | "LIMITED" | "L" => "LIMITED",
        "CORP" | "CORPN" | "CORPORATION" => "CORPORATION",
        "FIN" => "FINANCE",
        "DEV" => "DEVELOPMENT",
        "HSG" => "HOUSING",
        "CO" => "COMPANY",
        "PASS" => "PASSENGER",
        "VEH" => "VEHICLES",
        "PVT" => "PRIVATE",
        "INDS" | "INDUS" => "INDUSTRIES",
        "TECH" => "TECHNOLOGIES",
        other => other,
    }
}

/// Uppercase, expand `&`→AND, drop punctuation, expand common abbreviations,
/// collapse whitespace. Makes "HSG & URBAN DEV CORPN LTD" and "Housing & Urban
/// Development Corporation Limited" normalize to the same string.
pub fn normalize_name(s: &str) -> String {
    let upper = s.to_uppercase().replace('&', " AND ");
    let cleaned: String = upper.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' }).collect();
    cleaned.split_whitespace().map(expand).collect::<Vec<_>>().join(" ")
}

const STOP: &[&str] = &["LIMITED", "THE", "AND"];
/// Minimum fuzzy score to consider a match, and the margin the best DISTINCT
/// symbol must beat the runner-up by (else it's ambiguous → unmatched).
const FUZZY_MIN: f64 = 0.60;
const FUZZY_MARGIN: f64 = 0.10;

fn significant_tokens(s: &str) -> Vec<String> {
    normalize_name(s).split_whitespace().filter(|t| !STOP.contains(t)).map(|t| t.to_string()).collect()
}

/// Name/symbol resolver built from the metadata parquet.
pub struct SymbolResolver {
    by_norm_name: HashMap<String, String>, // normalized name/longName -> symbol
    valid_symbols: HashSet<String>,        // known trading symbols (UPPER)
    sector_of: HashMap<String, String>,    // symbol -> sector
    tokens: Vec<(HashSet<String>, String)>, // (significant tokens, symbol) for fuzzy
}

impl SymbolResolver {
    /// Build from `<root>/symbol_metadata.parquet`. On any I/O error returns an
    /// empty resolver (every name then resolves "unmatched", passed through) —
    /// never panics, never blocks the analysis.
    pub fn load(conn: &Connection, root: &Path) -> Self {
        let mut r = SymbolResolver {
            by_norm_name: HashMap::new(),
            valid_symbols: HashSet::new(),
            sector_of: HashMap::new(),
            tokens: Vec::new(),
        };
        let path = root.join("symbol_metadata.parquet");
        if !path.exists() {
            return r;
        }
        let sql = format!(
            "SELECT symbol, COALESCE(kite_symbol,''), COALESCE(name,''), COALESCE(longName,''), COALESCE(sector,'') \
             FROM read_parquet({}) ORDER BY symbol",
            crate::storage_kernel::quote_path(&path)
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return r,
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(_) => return r,
        };
        for row in rows.flatten() {
            let (symbol, kite, name, long, sector) = row;
            let sym = symbol.trim().to_uppercase();
            if sym.is_empty() {
                continue;
            }
            r.valid_symbols.insert(sym.clone());
            // kite_symbol like "NSE:IOC" — keep the bare symbol too.
            let kite_bare = kite.trim().trim_start_matches("NSE:").to_uppercase();
            if !kite_bare.is_empty() {
                r.valid_symbols.insert(kite_bare);
            }
            if !sector.trim().is_empty() {
                r.sector_of.insert(sym.clone(), sector.trim().to_string());
            }
            for cand in [name.as_str(), long.as_str()] {
                if cand.trim().is_empty() {
                    continue;
                }
                let norm = normalize_name(cand);
                if !norm.is_empty() {
                    r.by_norm_name.entry(norm).or_insert_with(|| sym.clone());
                    let toks = significant_tokens(cand);
                    if !toks.is_empty() {
                        r.tokens.push((toks.into_iter().collect(), sym.clone()));
                    }
                }
            }
        }
        r
    }

    fn sector(&self, sym: &str) -> Option<String> {
        self.sector_of.get(sym).cloned()
    }

    /// A SYMBOL → sector map (clone). Used by the Live Trade Plan's per-sector
    /// diversification cap. Empty when metadata is absent (cap then disabled).
    pub fn sector_map(&self) -> std::collections::HashMap<String, String> {
        self.sector_of.clone()
    }

    /// Resolve a raw statement name (+ optional ISIN) to a trading symbol.
    pub fn resolve(&self, raw: &str, _isin: Option<&str>) -> Resolved {
        let raw_up = raw.trim().to_uppercase();
        if raw_up.is_empty() {
            return Resolved { symbol: raw_up, sector: None, matched: false, how: "unmatched" };
        }
        // (1) already a known trading symbol (broker CSV exports).
        if self.valid_symbols.contains(&raw_up) {
            return Resolved { symbol: raw_up.clone(), sector: self.sector(&raw_up), matched: true, how: "symbol" };
        }
        // (2) exact normalized-name match.
        let norm = normalize_name(raw);
        if let Some(sym) = self.by_norm_name.get(&norm) {
            return Resolved { symbol: sym.clone(), sector: self.sector(sym), matched: true, how: "name" };
        }
        // (3) token-set fuzzy match (Jaccard + statement-token coverage), but ONLY
        // when there is a clear winner: the best DISTINCT symbol must beat the
        // runner-up by a margin. Otherwise (e.g. "UNION BANK" ~ City Union Bank vs
        // Union Bank of India; a bare group token like "TATA") it is ambiguous and
        // we refuse to guess — better unmatched-and-flagged than the wrong stock.
        let want: HashSet<String> = significant_tokens(raw).into_iter().collect();
        if !want.is_empty() {
            // best fuzzy score per DISTINCT symbol (a name can appear via name + longName)
            let mut by_sym: HashMap<&String, f64> = HashMap::new();
            for (cand, sym) in &self.tokens {
                let inter = want.intersection(cand).count();
                if inter == 0 {
                    continue;
                }
                let union = want.union(cand).count();
                let score = 0.5 * (inter as f64 / union as f64) + 0.5 * (inter as f64 / want.len() as f64);
                let e = by_sym.entry(sym).or_insert(0.0);
                if score > *e {
                    *e = score;
                }
            }
            let mut best: Option<(&String, f64)> = None;
            let mut runner = 0.0_f64;
            for (sym, &score) in &by_sym {
                match best {
                    Some((_, bs)) if score > bs => {
                        runner = bs;
                        best = Some((sym, score));
                    }
                    Some(_) if score > runner => runner = score,
                    None => best = Some((sym, score)),
                    _ => {}
                }
            }
            if let Some((sym, bs)) = best {
                if bs >= FUZZY_MIN && (by_sym.len() == 1 || bs - runner >= FUZZY_MARGIN) {
                    return Resolved { symbol: (*sym).clone(), sector: self.sector(sym), matched: true, how: "fuzzy" };
                }
            }
        }
        // (4) no confident match — keep the raw name, flagged.
        Resolved { symbol: raw_up, sector: None, matched: false, how: "unmatched" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_expands_abbreviations() {
        assert_eq!(normalize_name("HSG & URBAN DEV CORPN LTD"), "HOUSING AND URBAN DEVELOPMENT CORPORATION LIMITED");
        assert_eq!(normalize_name("INDIAN RAILWAY FIN CORP L"), "INDIAN RAILWAY FINANCE CORPORATION LIMITED");
        assert_eq!(normalize_name("TATA MOTORS PASS VEH LTD"), "TATA MOTORS PASSENGER VEHICLES LIMITED");
    }

    fn resolver_with(entries: &[(&str, &str, &str)]) -> SymbolResolver {
        // (symbol, name, sector)
        let mut r = SymbolResolver {
            by_norm_name: HashMap::new(),
            valid_symbols: HashSet::new(),
            sector_of: HashMap::new(),
            tokens: Vec::new(),
        };
        for (sym, name, sector) in entries {
            let s = sym.to_uppercase();
            r.valid_symbols.insert(s.clone());
            r.sector_of.insert(s.clone(), sector.to_string());
            r.by_norm_name.insert(normalize_name(name), s.clone());
            r.tokens.push((significant_tokens(name).into_iter().collect(), s));
        }
        r
    }

    #[test]
    fn resolves_company_name_exact_and_passes_through_symbol() {
        let r = resolver_with(&[
            ("HUDCO", "Housing & Urban Development Corporation Limited", "Financial Services"),
            ("TMCV", "Tata Motors Limited", "Consumer Cyclical"),
            ("IOC", "Indian Oil Corporation Limited", "Energy"),
        ]);
        let a = r.resolve("HSG & URBAN DEV CORPN LTD", None);
        assert_eq!(a.symbol, "HUDCO");
        assert!(a.matched && a.how == "name");
        assert_eq!(a.sector.as_deref(), Some("Financial Services"));
        // Tata Motors Limited -> TMCV (not TATAMOTORS).
        assert_eq!(r.resolve("TATA MOTORS LIMITED", None).symbol, "TMCV");
        // a raw trading symbol passes straight through.
        let p = r.resolve("IOC", None);
        assert_eq!(p.symbol, "IOC");
        assert_eq!(p.how, "symbol");
    }

    #[test]
    fn unmatched_keeps_raw_and_flags() {
        let r = resolver_with(&[("IOC", "Indian Oil Corporation Limited", "Energy")]);
        let u = r.resolve("SOME UNKNOWN PVT LTD", None);
        assert!(!u.matched);
        assert_eq!(u.how, "unmatched");
        assert_eq!(u.symbol, "SOME UNKNOWN PVT LTD");
    }

    #[test]
    fn ambiguous_fuzzy_refuses_to_guess() {
        // "UNION BANK" is close to BOTH City Union Bank and Union Bank of India;
        // within the margin ⇒ refuse (unmatched), never silently the wrong bank.
        let r = resolver_with(&[
            ("CUB", "City Union Bank Limited", "Financial Services"),
            ("UNIONBANK", "Union Bank of India", "Financial Services"),
            ("IOC", "Indian Oil Corporation Limited", "Energy"),
        ]);
        let a = r.resolve("UNION BANK", None);
        assert!(!a.matched, "ambiguous ⇒ unmatched, got {} ({})", a.symbol, a.how);
        // A single bare group token is also ambiguous across the group.
        let g = resolver_with(&[
            ("TATASTEEL", "Tata Steel Limited", "Materials"),
            ("TATAPOWER", "Tata Power Company Limited", "Utilities"),
        ]);
        assert!(!g.resolve("TATA", None).matched, "bare 'TATA' is ambiguous");
    }

    #[test]
    fn clear_fuzzy_winner_still_resolves() {
        // A confident single-winner fuzzy match (extra noise token) still works.
        let r = resolver_with(&[("SUZLON", "Suzlon Energy Limited", "Industrials")]);
        let a = r.resolve("SUZLON ENERGY CO LTD", None);
        assert!(a.matched && a.symbol == "SUZLON");
    }
}
