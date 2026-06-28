#!/usr/bin/env python3
"""
download_stock.py — fetch ONE NSE stock's full history into the parquet archive
(DuckDB-ready), matching the existing per-timeframe layout/schema exactly.

Writes:
  daily/{SYM}.parquet                         max daily OHLCV from Yahoo (~20 yr)
  minute/{SYM}.parquet                        1-min from Kite (from 2015, resumable)
  3min,5min,10min,15min,30min,60min,1day      resampled from the 1-min series

Reuses the proven methods from 02_download_daily_yahoo.py + 03_download_1min_zerodha.py.
Kite token resolution prefers today's cached `.kite_token.json` (the morning-minted
token), then env KITE_ACCESS_TOKEN. If neither is valid, Yahoo daily still completes
and the intraday step reports "needs a fresh Kite token" — it never fabricates data.

The LAST line of stdout is a single JSON object the app parses; human-readable
progress goes to stderr.

Usage:
  python3 download_stock.py 63MOONS --root /path/to/1500-Stocks-Parquest
  python3 download_stock.py 63MOONS --root ... --yahoo-only
"""
import argparse, json, os, sys, time
from datetime import datetime, timedelta
from zoneinfo import ZoneInfo
import pandas as pd

IST = ZoneInfo("Asia/Kolkata")
KITE_START = "2015-02-01"
RESAMPLE = {"3min": "3min", "5min": "5min", "10min": "10min",
            "15min": "15min", "30min": "30min", "60min": "60min", "1day": "1D"}
AGG = {"open": "first", "high": "max", "low": "min", "close": "last", "volume": "sum"}
RATE_SLEEP = 0.35
WINDOW_DAYS = 60


def log(*a):
    print(*a, file=sys.stderr, flush=True)


def load_env(repo_root):
    """Load KITE_* from the repo's .env (the Rust app reads it the same way)."""
    envp = os.path.join(repo_root, ".env")
    if os.path.exists(envp):
        for line in open(envp):
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                os.environ.setdefault(k.strip(), v.strip())


def resolve_kite(archive_root):
    """Return (KiteConnect, None) if a valid token exists, else (None, reason).
    Prefers today's cached token over a possibly-stale env token."""
    try:
        from kiteconnect import KiteConnect
    except Exception as e:
        return None, f"kiteconnect not installed ({e})"
    api_key = os.environ.get("KITE_API_KEY")
    if not api_key:
        return None, "KITE_API_KEY not set"
    today = datetime.now(IST).strftime("%Y-%m-%d")
    tok = None
    cache = os.path.join(archive_root, ".kite_token.json")
    if os.path.exists(cache):
        try:
            c = json.load(open(cache))
            if c.get("api_key") == api_key and c.get("date") == today:
                tok = c.get("access_token")
        except Exception:
            pass
    tok = tok or os.environ.get("KITE_ACCESS_TOKEN")
    if not tok:
        return None, "no Kite token — run `python3 kite_auth.py` to mint today's token"
    k = KiteConnect(api_key=api_key)
    k.set_access_token(tok)
    try:
        k.profile()  # validate (cheap)
        return k, None
    except Exception as e:
        return None, f"Kite token invalid/expired ({type(e).__name__}) — re-auth (tokens expire ~07:30 IST daily)"


def fetch_minute(kite, token, start, end):
    """Chunked 1-min fetch (60-day windows, rate-limited). Mirrors 03_download."""
    frames, cur = [], start
    while cur < end:
        win_end = min(cur + timedelta(days=WINDOW_DAYS), end)
        for attempt in range(4):
            try:
                data = kite.historical_data(token, cur, win_end, "minute")
                if data:
                    frames.append(pd.DataFrame(data))
                break
            except Exception as e:
                wait = 2 ** attempt
                log(f"      retry ({type(e).__name__}) in {wait}s")
                time.sleep(wait)
        time.sleep(RATE_SLEEP)
        cur = win_end + timedelta(days=1)
    if not frames:
        return pd.DataFrame()
    df = pd.concat(frames, ignore_index=True)
    df["date"] = pd.to_datetime(df["date"])
    df = df.set_index("date").sort_index()
    df = df[~df.index.duplicated(keep="first")]
    return df[["open", "high", "low", "close", "volume"]]


def resample_all(root, symbol, minute_df):
    """Build 3/5/10/15/30/60-min + 1day from 1-min, anchored to the 09:15 NSE open
    (offset 555min) — byte-compatible with the existing archive."""
    written = []
    for tf, rule in RESAMPLE.items():
        os.makedirs(os.path.join(root, tf), exist_ok=True)
        if tf == "1day":
            out = minute_df.resample("1D").agg(AGG).dropna(how="any")
        else:
            out = (minute_df.resample(rule, label="left", closed="left",
                   origin="start_day", offset="555min").agg(AGG).dropna(how="any"))
        out.to_parquet(os.path.join(root, tf, f"{symbol}.parquet"))
        written.append({"tf": tf, "rows": int(len(out))})
    return written


def yahoo_daily(root, symbol):
    """Max daily OHLCV from Yahoo → daily/{SYM}.parquet. Mirrors 02_download."""
    import yfinance as yf
    os.makedirs(os.path.join(root, "daily"), exist_ok=True)
    df = yf.download(f"{symbol}.NS", period="max", interval="1d",
                     auto_adjust=False, progress=False, threads=False)
    if df is None or df.empty:
        return {"status": "empty", "rows": 0}
    if isinstance(df.columns, pd.MultiIndex):
        df.columns = df.columns.get_level_values(0)
    df = df.rename(columns=str.lower)
    df.index.name = "date"
    df.to_parquet(os.path.join(root, "daily", f"{symbol}.parquet"))
    return {"status": "ok", "rows": int(len(df)),
            "start": str(df.index.min().date()), "end": str(df.index.max().date())}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("symbol")
    ap.add_argument("--root", required=True, help="the 1500-Stocks-Parquest archive dir")
    ap.add_argument("--start", default=KITE_START)
    ap.add_argument("--yahoo-only", action="store_true")
    args = ap.parse_args()

    sym = args.symbol.strip().upper()
    root = os.path.abspath(args.root)
    repo_root = os.path.dirname(root)
    result = {"symbol": sym, "root": root, "daily": None, "intraday": None,
              "kite_ok": False, "messages": []}

    if not sym or not all(c.isalnum() or c in "&-" for c in sym):
        result["messages"].append("invalid symbol")
        print(json.dumps(result))
        return

    load_env(repo_root)

    # 1) Yahoo daily (max history) — works without auth.
    log(f"[{sym}] Yahoo daily (max)…")
    try:
        result["daily"] = yahoo_daily(root, sym)
        log(f"      daily: {result['daily']}")
    except Exception as e:
        result["daily"] = {"status": "error", "error": str(e)[:200]}

    # 2) Kite intraday (1-min → resampled).
    if not args.yahoo_only:
        kite, reason = resolve_kite(root)
        if kite is None:
            result["intraday"] = {"status": "skipped", "reason": reason}
            result["messages"].append(reason)
            log(f"      intraday SKIPPED: {reason}")
        else:
            result["kite_ok"] = True
            try:
                log(f"[{sym}] Kite instruments…")
                inst = pd.DataFrame(kite.instruments("NSE"))
                inst = inst[inst["segment"] == "NSE"]
                row = inst[inst["tradingsymbol"] == sym]
                if not len(row):
                    result["intraday"] = {"status": "error", "error": f"{sym}: no NSE instrument token"}
                else:
                    token = int(row.iloc[0]["instrument_token"])
                    os.makedirs(os.path.join(root, "minute"), exist_ok=True)
                    path = os.path.join(root, "minute", f"{sym}.parquet")
                    existing = pd.read_parquet(path) if os.path.exists(path) else None
                    start = datetime.strptime(args.start, "%Y-%m-%d").replace(tzinfo=IST)
                    end = datetime.now(IST)
                    fetch_from = (existing.index.max() + timedelta(days=1)
                                  if existing is not None and len(existing) else start)
                    log(f"[{sym}] minute fetch {fetch_from.date()} → {end.date()}…")
                    if fetch_from < end:
                        new = fetch_minute(kite, token, fetch_from, end)
                        df = pd.concat([existing, new]) if existing is not None else new
                        df = df[~df.index.duplicated(keep="first")].sort_index()
                    else:
                        df = existing
                    if df is not None and len(df):
                        df.to_parquet(path)
                        written = resample_all(root, sym, df)
                        result["intraday"] = {
                            "status": "ok", "minute_rows": int(len(df)),
                            "start": str(df.index.min()), "end": str(df.index.max()),
                            "resampled": written,
                        }
                    else:
                        result["intraday"] = {"status": "empty", "minute_rows": 0}
                log(f"      intraday: {result['intraday'].get('status')}")
            except Exception as e:
                result["intraday"] = {"status": "error", "error": str(e)[:200]}

    print(json.dumps(result))


if __name__ == "__main__":
    main()
