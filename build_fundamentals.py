#!/usr/bin/env python3
"""
build_fundamentals.py — flatten indianapi/stock/*.json into ONE numeric
fundamentals.parquet the Rust app can read for a display-only context panel.

The indianapi `/stock` payload nests ratios under keyMetrics → {category} →
[{key, value}], with messy keys (typos, stray spaces/parens), values as strings.
We normalize each key (lowercase, strip non-alphanumeric) and pick a small set of
canonical fields by exact normalized match (with documented fallbacks). Promoter
holding comes from the `shareholding` list; company/as_of from the snapshot.

Output: fundamentals.parquet (one row per covered symbol). Columns are numeric
(NaN when a source value is missing — never fabricated).

Honesty: this is DISPLAY-ONLY context. The Rust loader is firewalled and these
numbers NEVER enter the eligibility gate, Confidence, ranking, or sizing.

Usage:
  python3 build_fundamentals.py --root /path/to/1500-Stocks-Parquest          # full rebuild
  python3 build_fundamentals.py --root ... --symbol 63MOONS                    # upsert one
"""
import argparse, glob, json, os, re
import pandas as pd

# canonical field -> ordered list of acceptable NORMALIZED keys (first hit wins).
FIELD_KEYS = {
    "pe": ["pperebasicexcludingextraordinaryitemsttm",
           "ppereexcludingextraordinaryitemsmostrecentfiscalyear",
           "pperenormalizedmostrecentfiscalyear"],
    "roe": ["returnonaverageequitytrailing12month",
            "returnonaverageequitymostrecentfiscalyear",
            "returnonaverageequity5yearaverage"],
    "debt_to_equity": ["totaldebtpertotalequitymostrecentquarter",
                        "totaldebtpertotalequitymostrecentfiscalyear",
                        "ltdebtperequitymostrecentquarter"],
    "peg": ["pegratio"],
    "rev_growth_5y": ["revenuegrowthrate5year", "growthratepercentrevenue3year"],
    "eps_growth_5y": ["epsgrowthrate5year", "growthratepercenteps3year"],
    "profit_margin": ["netprofitmarginpercenttrailing12month",
                      "netprofitmargin5yearaverage"],
    "dividend_yield": ["currentdividendyieldcommonstockprimaryissueltm",
                       "dividendyieldindicatedannualdividenddividedbyclosingprice"],
    "price_to_book": ["pricetobookmostrecentquarter", "pricetobookmostrecentfiscalyear"],
    "market_cap_cr": ["marketcap"],
}


def norm(k):
    return re.sub(r"[^a-z0-9]", "", str(k).lower())


def to_float(v):
    if v is None:
        return None
    try:
        f = float(str(v).replace(",", "").strip())
        return f if f == f else None  # drop NaN
    except (ValueError, TypeError):
        return None


def flatten_keymetrics(jobj):
    """All keyMetrics {normalized_key: value} across every category."""
    out = {}
    km = jobj.get("keyMetrics") or {}
    if isinstance(km, dict):
        for _cat, items in km.items():
            if isinstance(items, list):
                for it in items:
                    if isinstance(it, dict) and "key" in it:
                        out[norm(it["key"])] = it.get("value")
    return out


def promoter_pct(jobj):
    """Latest promoter holding % from the `shareholding` list (or None)."""
    sh = jobj.get("shareholding")
    if not isinstance(sh, list):
        return None
    for cat in sh:
        if not isinstance(cat, dict):
            continue
        name = (cat.get("displayName") or cat.get("categoryName") or "").lower()
        if "promoter" in name and "non" not in name and "public" not in name:
            cats = cat.get("categories")
            if isinstance(cats, list) and cats:
                # latest by holdingDate
                latest = sorted(cats, key=lambda c: str(c.get("holdingDate", "")))[-1]
                return to_float(latest.get("percentage"))
    return None


def flatten_one(symbol, jobj):
    """One fundamentals row from a parsed /stock payload, or None if unusable."""
    if not isinstance(jobj, dict):
        return None
    km = flatten_keymetrics(jobj)
    row = {"symbol": symbol.upper()}
    for field, keys in FIELD_KEYS.items():
        val = None
        for k in keys:
            if k in km:
                val = to_float(km[k])
                if val is not None:
                    break
        row[field] = val
    row["promoter_pct"] = promoter_pct(jobj)
    row["company"] = jobj.get("companyName")
    snap = jobj.get("stockDetailsReusableData") or {}
    row["as_of"] = snap.get("date") if isinstance(snap, dict) else None
    # Require at least one real numeric field, else it is an empty/errored payload.
    numeric = [row.get(f) for f in list(FIELD_KEYS) + ["promoter_pct"]]
    if not any(v is not None for v in numeric):
        return None
    return row


COLS = ["symbol", "company", "pe", "roe", "debt_to_equity", "peg",
        "rev_growth_5y", "eps_growth_5y", "profit_margin", "dividend_yield",
        "price_to_book", "promoter_pct", "market_cap_cr", "as_of"]


def write_parquet(df, path):
    tmp = path + ".tmp"
    df[COLS].to_parquet(tmp, index=False)
    os.replace(tmp, path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--symbol", default=None, help="upsert just this symbol (else full rebuild)")
    args = ap.parse_args()
    root = os.path.abspath(args.root)
    stock_dir = os.path.join(root, "indianapi", "stock")
    out_path = os.path.join(root, "fundamentals.parquet")

    if args.symbol:
        sym = args.symbol.strip().upper()
        jp = os.path.join(stock_dir, f"{sym}.json")
        if not os.path.exists(jp):
            print(json.dumps({"status": "skipped", "reason": f"no indianapi JSON for {sym}"}))
            return
        try:
            row = flatten_one(sym, json.load(open(jp)))
        except Exception as e:
            print(json.dumps({"status": "error", "error": str(e)[:160]}))
            return
        if row is None:
            print(json.dumps({"status": "skipped", "reason": f"{sym}: payload had no usable numeric fields"}))
            return
        existing = pd.read_parquet(out_path) if os.path.exists(out_path) else pd.DataFrame(columns=COLS)
        existing = existing[existing["symbol"].astype(str).str.upper() != sym]
        df = pd.concat([existing, pd.DataFrame([row])], ignore_index=True)
        write_parquet(df, out_path)
        print(json.dumps({"status": "ok", "action": "upserted", "symbol": sym,
                          "fields": {k: row.get(k) for k in ["pe", "roe", "debt_to_equity", "promoter_pct"]}}))
        return

    # full rebuild
    rows, skipped = [], 0
    for jp in sorted(glob.glob(os.path.join(stock_dir, "*.json"))):
        sym = os.path.basename(jp)[:-5].upper()
        try:
            row = flatten_one(sym, json.load(open(jp)))
        except Exception:
            row = None
        if row is None:
            skipped += 1
        else:
            rows.append(row)
    df = pd.DataFrame(rows, columns=COLS) if rows else pd.DataFrame(columns=COLS)
    write_parquet(df, out_path)
    print(json.dumps({"status": "ok", "symbols": len(rows), "skipped": skipped,
                      "out": out_path}))


if __name__ == "__main__":
    main()
