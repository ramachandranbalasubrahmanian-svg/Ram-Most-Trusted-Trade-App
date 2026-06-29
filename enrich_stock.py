#!/usr/bin/env python3
"""
enrich_stock.py — onboard ONE NSE stock's *details* (not candles) into the archive.

This is the sibling of `download_stock.py`: where that fetches OHLCV candles,
this fetches the reference + fundamental detail that the dashboards display, so a
freshly-added stock is not a nameless, sectorless, fundamentals-less row.

Writes / upserts (each step independent — one failing never aborts the rest):
  symbol_metadata.parquet            upsert this symbol's row (sector/industry/
                                     mcap/name/isin) from Yahoo `.info`, with a
                                     recent_listings.csv fallback for name/mcap
  corp_actions/{SYM}.parquet         per-stock dividends + splits (long format)
  corporate_actions_all.parquet      upsert (replace this symbol's rows only)
  daily_adj/{SYM}.parquet            split/bonus back-adjusted daily slice
                                     (raw daily/ kept intact) — display data only
  indianapi/stock/{SYM}.json         fundamentals snapshot — ONLY if INDIANAPI_KEY
                                     is set (a paid endpoint); otherwise skipped
                                     with an honest reason, never fabricated

Method matches the existing pipeline scripts byte-for-byte:
  - metadata FIELDS + schema  ← 07_metadata.py
  - corp-action long format   ← 05_corporate_actions.py
  - split/bonus back-adjust   ← 10_adjust_prices.py
  - indianapi /stock endpoint ← 11_indianapi_fundamentals.py

Honesty-first: this is DISPLAY-ONLY reference data. It never touches the edge
map, the eligibility gate, Confidence, or any backtest — the Rust intraday
backtest reads the RAW resampled candles, not daily_adj/. Nothing here can move
an anchor.

Atomic parquet writes (temp file + os.replace) so an interrupted run can never
corrupt symbol_metadata.parquet / corporate_actions_all.parquet.

The LAST line of stdout is a single JSON object the app parses; human-readable
progress goes to stderr.

Usage:
  python3 enrich_stock.py 63MOONS --root /path/to/1500-Stocks-Parquest
  python3 enrich_stock.py 63MOONS --root ... --no-fundamentals
"""
import argparse, json, os, sys
import pandas as pd

# Yahoo `.info` fields we persist into symbol_metadata (mirrors 07_metadata.py).
META_FIELDS = ["sector", "industry", "isin", "marketCap", "sharesOutstanding", "longName"]
INDIANAPI_BASE = "https://stock.indianapi.in"


def log(*a):
    print(*a, file=sys.stderr, flush=True)


def load_env(repo_root):
    """Load KITE_*/INDIANAPI_KEY from the repo's .env (same as download_stock.py)."""
    envp = os.path.join(repo_root, ".env")
    if os.path.exists(envp):
        for line in open(envp):
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                os.environ.setdefault(k.strip(), v.strip())


def atomic_write_parquet(df, path):
    """Write a parquet atomically (temp in the same dir + os.replace), so a crash
    mid-write can never leave a half-written reference table on disk."""
    tmp = path + ".tmp"
    df.to_parquet(tmp)
    os.replace(tmp, path)


# ----------------------------------------------------------------------------
# 1) symbol_metadata upsert
# ----------------------------------------------------------------------------
def recent_listings_row(root, sym):
    """Fallback name/mcap/yahoo/kite from recent_listings.csv (no network)."""
    p = os.path.join(root, "recent_listings.csv")
    if not os.path.exists(p):
        return None
    try:
        rl = pd.read_csv(p)
        hit = rl[rl["symbol"].astype(str).str.upper() == sym]
        if len(hit):
            return hit.iloc[0].to_dict()
    except Exception:
        pass
    return None


def yahoo_info(sym):
    """Yahoo `.info` for the metadata fields. Returns {} on any failure (no fabrication)."""
    try:
        import yfinance as yf
        i = yf.Ticker(f"{sym}.NS").info or {}
        return {k: i.get(k) for k in META_FIELDS}
    except Exception as e:
        log(f"      yahoo .info failed: {type(e).__name__}: {str(e)[:120]}")
        return {}


def upsert_metadata(root, sym):
    """Upsert this symbol's row in symbol_metadata.parquet. Replace an existing
    row in place (preserving its rank); else append with a fresh rank. Never
    deletes or reorders other rows. Returns a status dict."""
    path = os.path.join(root, "symbol_metadata.parquet")
    info = yahoo_info(sym)
    rl = recent_listings_row(root, sym) or {}

    existing = None
    if os.path.exists(path):
        meta = pd.read_parquet(path)
    else:
        # Materialize a metadata table from scratch with the canonical columns.
        meta = pd.DataFrame(columns=[
            "rank", "symbol", "yahoo_symbol", "kite_symbol", "name", "market_cap_inr",
            "sector", "industry", "isin", "marketCap", "sharesOutstanding", "longName"])

    mask = meta["symbol"].astype(str).str.upper() == sym if len(meta) else pd.Series([], dtype=bool)
    if len(meta) and mask.any():
        existing = meta[mask].iloc[0].to_dict()

    # Build the upserted row: prefer fresh Yahoo info, then the prior row, then
    # recent_listings, then a safe default. Never invent a sector.
    def pick(*vals):
        for v in vals:
            if v is not None and not (isinstance(v, float) and pd.isna(v)) and str(v) != "":
                return v
        return None

    name = pick(info.get("longName"), (existing or {}).get("name"), rl.get("name"), sym)
    rank = pick((existing or {}).get("rank"), rl.get("rank"))
    if rank is None:
        rank = (int(pd.to_numeric(meta["rank"], errors="coerce").max()) + 1) if len(meta) else 1
    market_cap = pick(info.get("marketCap"), (existing or {}).get("market_cap_inr"), rl.get("market_cap_inr"))

    row = {
        "rank": rank,
        "symbol": sym,
        "yahoo_symbol": pick((existing or {}).get("yahoo_symbol"), rl.get("yahoo_symbol"), f"{sym}.NS"),
        "kite_symbol": pick((existing or {}).get("kite_symbol"), rl.get("kite_symbol"), f"NSE:{sym}"),
        "name": name,
        "market_cap_inr": market_cap,
        "sector": pick(info.get("sector"), (existing or {}).get("sector")),
        "industry": pick(info.get("industry"), (existing or {}).get("industry")),
        "isin": pick(info.get("isin"), (existing or {}).get("isin")),
        "marketCap": pick(info.get("marketCap"), (existing or {}).get("marketCap")),
        "sharesOutstanding": pick(info.get("sharesOutstanding"), (existing or {}).get("sharesOutstanding")),
        "longName": pick(info.get("longName"), (existing or {}).get("longName"), name),
    }

    # Drop any prior row for this symbol, append the upserted one (same column order).
    if len(meta) and mask.any():
        meta = meta[~mask]
    meta = pd.concat([meta, pd.DataFrame([row])[meta.columns]], ignore_index=True)
    atomic_write_parquet(meta, path)

    return {
        "status": "ok",
        "action": "updated" if existing else "inserted",
        "sector": row["sector"],
        "industry": row["industry"],
        "name": row["name"],
        "market_cap_inr": row["market_cap_inr"],
        "yahoo_info": any(v is not None for v in info.values()),
    }


# ----------------------------------------------------------------------------
# 2) corporate actions (per-stock + combined upsert)
# ----------------------------------------------------------------------------
def fetch_corp_actions(sym):
    """Dividends + splits from Yahoo → long-format DataFrame [date,type,value].
    Mirrors 05_corporate_actions.py. Returns (df, error|None)."""
    try:
        import yfinance as yf
        t = yf.Ticker(f"{sym}.NS")
        rows = []
        div = t.dividends   # pandas Series (may be empty); never use in a bool `or`
        spl = t.splits
        if div is not None:
            for d, v in div.items():
                rows.append((d, "dividend", float(v)))
        if spl is not None:
            for d, v in spl.items():
                rows.append((d, "split", float(v)))
        df = pd.DataFrame(rows, columns=["date", "type", "value"])
        return df, None
    except Exception as e:
        return None, f"{type(e).__name__}: {str(e)[:120]}"


def upsert_corp_actions(root, sym):
    """Write corp_actions/{SYM}.parquet and upsert the combined table (replacing
    only this symbol's rows). Returns a status dict + the symbol's splits/divs for
    the daily_adj step."""
    df, err = fetch_corp_actions(sym)
    if df is None:
        return {"status": "error", "error": err}, [], []

    # Per-stock file (always write — empty is a valid 'no actions' result).
    od = os.path.join(root, "corp_actions")
    os.makedirs(od, exist_ok=True)
    atomic_write_parquet(df, os.path.join(od, f"{sym}.parquet"))

    # Combined upsert.
    cpath = os.path.join(root, "corporate_actions_all.parquet")
    sym_df = df.copy()
    sym_df.insert(0, "symbol", sym)
    if os.path.exists(cpath):
        allca = pd.read_parquet(cpath)
        allca = allca[allca["symbol"].astype(str).str.upper() != sym]
        # Align dtypes for a clean concat (combined 'date' is tz-aware datetime).
        if len(allca):
            sym_df["date"] = pd.to_datetime(sym_df["date"], utc=False)
        allca = pd.concat([allca, sym_df[allca.columns]], ignore_index=True)
    else:
        allca = sym_df
    atomic_write_parquet(allca, cpath)

    # Normalized (tz-naive, day) splits/divs for back-adjustment.
    def norm(g):
        out = []
        for d, v in zip(g["date"], g["value"]):
            ts = pd.to_datetime(d)
            if ts.tzinfo is not None:
                ts = ts.tz_localize(None)
            out.append((ts.normalize(), float(v)))
        return out

    splits = norm(df[df["type"] == "split"])
    divs = norm(df[df["type"] == "dividend"])
    return {"status": "ok", "n_dividends": len(divs), "n_splits": len(splits)}, splits, divs


# ----------------------------------------------------------------------------
# 3) daily_adj slice (split/bonus back-adjust) — display data only
# ----------------------------------------------------------------------------
PRICE_COLS = ["open", "high", "low", "close"]


def back_adjust(df, splits):
    """Industry-standard back-adjust for splits/bonuses (latest bars unchanged,
    history scaled). Mirrors 10_adjust_prices.py (split/bonus only)."""
    idx = df.index.tz_localize(None) if df.index.tz is not None else df.index
    pf = pd.Series(1.0, index=df.index)  # price factor
    vf = pd.Series(1.0, index=df.index)  # volume factor
    for ex, r in splits:
        if r and r > 0:
            m = idx < ex
            pf[m] /= r
            vf[m] *= r
    out = df.copy()
    for c in PRICE_COLS:
        if c in out.columns:
            out[c] = out[c] * pf
    if "volume" in out.columns:
        out["volume"] = (out["volume"] * vf).round().astype("int64")
    return out


def write_daily_adj(root, sym, splits):
    """Back-adjust daily/{SYM}.parquet → daily_adj/{SYM}.parquet. If there are no
    splits, the adjusted slice equals the raw daily (still written, so coverage is
    complete). Returns a status dict."""
    src = os.path.join(root, "daily", f"{sym}.parquet")
    if not os.path.exists(src):
        return {"status": "skipped", "reason": "no daily candles yet (run download_stock first)"}
    try:
        df = pd.read_parquet(src)
        if "date" in df.columns:
            df = df.set_index("date")
        df.index = pd.to_datetime(df.index)
        dst_dir = os.path.join(root, "daily_adj")
        os.makedirs(dst_dir, exist_ok=True)
        adj = back_adjust(df, splits) if splits else df
        atomic_write_parquet(adj, os.path.join(dst_dir, f"{sym}.parquet"))
        return {"status": "ok", "rows": int(len(adj)), "splits_applied": len(splits)}
    except Exception as e:
        return {"status": "error", "error": f"{type(e).__name__}: {str(e)[:120]}"}


# ----------------------------------------------------------------------------
# 4) indianapi fundamentals snapshot (paid; opt-in via key presence)
# ----------------------------------------------------------------------------
def fetch_fundamentals(root, sym):
    """GET /stock?name=SYM from indianapi.in → indianapi/stock/{SYM}.json.
    Only runs if INDIANAPI_KEY is set (a paid endpoint). Mirrors
    11_indianapi_fundamentals.py. Honest skip otherwise — never fabricated."""
    key = os.environ.get("INDIANAPI_KEY")
    if not key:
        return {"status": "skipped", "reason": "INDIANAPI_KEY not set — fundamentals are a paid endpoint, not fetched"}
    try:
        import requests
    except Exception as e:
        return {"status": "skipped", "reason": f"requests not installed ({e})"}
    try:
        s = requests.Session()
        s.headers.update({"X-API-Key": key})
        r = s.get(f"{INDIANAPI_BASE}/stock", params={"name": sym}, timeout=30)
        r.raise_for_status()
        obj = r.json()
        od = os.path.join(root, "indianapi", "stock")
        os.makedirs(od, exist_ok=True)
        tmp = os.path.join(od, f"{sym}.json.tmp")
        json.dump(obj, open(tmp, "w"))
        os.replace(tmp, os.path.join(od, f"{sym}.json"))
        # Best-effort: also upsert this symbol's row into fundamentals.parquet so
        # it shows in the deep-dive panel immediately (no full rebuild needed).
        try:
            from build_fundamentals import flatten_one, COLS, write_parquet
            row = flatten_one(sym, obj)
            if row is not None:
                fp = os.path.join(root, "fundamentals.parquet")
                ex = pd.read_parquet(fp) if os.path.exists(fp) else pd.DataFrame(columns=COLS)
                ex = ex[ex["symbol"].astype(str).str.upper() != sym]
                df = pd.concat([ex, pd.DataFrame([row])[COLS]], ignore_index=True)
                write_parquet(df, fp)
        except Exception as e:
            log(f"      fundamentals parquet upsert skipped: {type(e).__name__}: {str(e)[:100]}")
        has = bool(isinstance(obj, dict) and obj.get("companyName"))
        return {"status": "ok", "company": (obj.get("companyName") if isinstance(obj, dict) else None),
                "has_financials": bool(isinstance(obj, dict) and obj.get("financials"))} if has else \
               {"status": "ok", "note": "saved (no companyName in payload)"}
    except Exception as e:
        return {"status": "error", "error": f"{type(e).__name__}: {str(e)[:160]}"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("symbol")
    ap.add_argument("--root", required=True, help="the 1500-Stocks-Parquest archive dir")
    ap.add_argument("--no-fundamentals", action="store_true",
                    help="skip the indianapi fundamentals call even if a key is set")
    args = ap.parse_args()

    sym = args.symbol.strip().upper()
    root = os.path.abspath(args.root)
    repo_root = os.path.dirname(root)
    result = {"symbol": sym, "root": root, "metadata": None, "corp_actions": None,
              "daily_adj": None, "fundamentals": None, "messages": []}

    if not sym or not all(c.isalnum() or c in "&-" for c in sym):
        result["messages"].append("invalid symbol")
        print(json.dumps(result))
        return

    load_env(repo_root)

    # 1) metadata
    log(f"[{sym}] symbol_metadata upsert…")
    try:
        result["metadata"] = upsert_metadata(root, sym)
    except Exception as e:
        result["metadata"] = {"status": "error", "error": str(e)[:200]}
    log(f"      metadata: {result['metadata']}")

    # 2) corp actions (feeds 3)
    log(f"[{sym}] corporate actions…")
    splits, divs = [], []
    try:
        result["corp_actions"], splits, divs = upsert_corp_actions(root, sym)
    except Exception as e:
        result["corp_actions"] = {"status": "error", "error": str(e)[:200]}
    log(f"      corp_actions: {result['corp_actions']}")

    # 3) daily_adj
    log(f"[{sym}] daily_adj slice…")
    try:
        result["daily_adj"] = write_daily_adj(root, sym, splits)
    except Exception as e:
        result["daily_adj"] = {"status": "error", "error": str(e)[:200]}
    log(f"      daily_adj: {result['daily_adj']}")

    # 4) fundamentals (optional)
    if args.no_fundamentals:
        result["fundamentals"] = {"status": "skipped", "reason": "--no-fundamentals"}
    else:
        log(f"[{sym}] indianapi fundamentals…")
        try:
            result["fundamentals"] = fetch_fundamentals(root, sym)
        except Exception as e:
            result["fundamentals"] = {"status": "error", "error": str(e)[:200]}
    log(f"      fundamentals: {result['fundamentals']}")

    print(json.dumps(result, default=str))


if __name__ == "__main__":
    main()
