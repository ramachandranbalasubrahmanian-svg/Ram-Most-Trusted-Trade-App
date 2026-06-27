# RAM_ISTP — Session Handover

A local, **signals-only** NSE intraday + swing analytics platform in **Rust**, querying a local
Parquet archive via **DuckDB**, serving three web dashboards. It never places broker orders — it
stages signals for manual execution and tracks them synthetically.

- **GitHub:** https://github.com/ramachandranbalasubrahmanian-svg/Ram-Most-Trusted-Trade-App
- **Local project folder:** `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`
- **Branch:** `main` · ~5,100+ LOC Rust + 3 HTML pages · 84 unit tests passing

## How to resume in a new session
Open the project folder above. The git repo and all source are there. Then:

```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
cargo build && cargo test          # ~84 tests should pass
./target/debug/ram_istp serve 30min   # dashboards at http://127.0.0.1:8787
```

> NOTE: the **14 GB data archive is NOT in git** (gitignored). It lives locally at
> `RAM_ISTP_Rust_Architecture/1500-Stocks-Parquest/` (folders: `minute/ 1day/ daily/ 5min/ 15min/
> 30min/ 60min/ 3min/ 10min/ index_daily/ …`, files `{SYMBOL}.parquet`). A **fresh clone elsewhere
> has no data** — keep using this folder, or point `RAM_ISTP_DATA_ROOT` at the archive.

## CLI commands
```bash
ram_istp premarket [SYMBOL...]      # pre-market scan: macro ATR/52w S-R + VAH/VAL
ram_istp backtest [TF] [SYMBOL...]  # backtest strategy library → cached edge map (cache/)
ram_istp serve [TF]                 # replay + all dashboards (default 30min)
ram_istp suggest SYMBOL             # per-stock intraday suggestion (CLI view of /intraday)
```

## Web pages (when `serve` is running on :8787)
- `/`          — **Live Signals** dashboard: Top-10 Buy/Sell, budget slider (₹5L,+₹50k), risk 1–7%,
  risk meter, diagnostics (tick→signal µs).
- `/intraday`  — **Intraday Suggestion**: per-stock 4-strategy deep-dive (Confidence/Conviction/DSR/
  Monte-Carlo/bootstrap), 10-Buy/10-Sell scanner, **Capital-Fit ATR Finder** subtab.
- `/desk`      — **Trading Desk**: signal-freeze + circuit breaker, staging console (copy/paste
  Bracket Orders), swing ledger, portfolio analytics, manual journal.

## Architecture (src/)
| File | Role |
|------|------|
| `config.rs` | paths, IST sessions, budget/risk, capital pool, risk tiers, all constants |
| `storage_kernel.rs` | DuckDB out-of-core parquet reads; pre-market scan (ATR/VAH-VAL); candle/date loaders |
| `strategy_engine.rs` | 13-strategy library + intraday backtester + edge map (cache JSON) |
| `stats.rs` | t-stat, Sharpe, Calmar, Wilson, Monte-Carlo, bootstrap CI, **DSR**, Confidence, Conviction |
| `validation.rs` | purged+embargoed OOS, walk-forward consistency, parameter robustness |
| `regime.rs` | NIFTY up/down regime map + regime-consistency check |
| `suggestion_engine.rs` | per-stock 240-config analysis + scanner + Capital-Fit finder + regime/breadth |
| `costs.rs` | itemized Indian intraday charges (brokerage/STT/txn/SEBI/GST/stamp + slippage) |
| `risk_manager.rs` | sizing + projected P&L + risk meter + 15:15 square-off alert |
| `analytics_kernel.rs` | per-symbol 1000-tick ring buffers, OBI/VWAP/z-score/RVOL, ranking |
| `ingestion_engine.rs` | replay simulator (default) + Kite WS binary parser + live client (Phase 5) |
| `execution_staging.rs` | SEBI-compliant LIMIT Bracket-Order staging (LTP ± ATR×0.1) |
| `circuit_breaker.rs` | synthetic MTM → Signal Freeze at −2% of capital pool |
| `journal_sync.rs` | DuckDB `manual_validation_journal_2026` + slippage/PnL + 15:45 CSV export |
| `portfolio_analytics.rs` | win%/PF/Sharpe/maxDD/equity-curve + attribution by strategy & sector |
| `news_engine.rs` | reactive Marketaux/EODHD sentiment (feature-flag OFF; mock default) |
| `server.rs` | Axum + `/ws/live_signals` + all `/api/*` routes |
| `main.rs` | tokio lifecycle: pre-market → ingestion/analytics/risk threads → server + desk scheduler |
| `types.rs` | all cross-module data contracts |
| `ui/{index,intraday,desk}.html` | the three Tailwind dashboards (zero-build, served at request time) |

## Honest invariants (KEEP THESE — this is a real-money, honesty-first project)
1. **Signals only.** Nothing ever places/modifies/cancels a broker order. The journal is synthetic.
2. **Net-of-cost truth.** All R/P&L are net of itemized costs (~0.13% round-trip backtest;
   exact per-trade on cards). Past edge ≠ future return — labeled as scenarios, never promises.
3. **Confidence vs Conviction** are separate; Confidence is t-stat-based + behavioural penalties +
   **DSR gate** (caps <60 unless Deflated-Sharpe ≥ 0.5). Honest finding: intraday net edge ≈ 0;
   the validated +EV edge is multi-day swing.
4. **15:15 square-off** is an ALERT, not an order.

## Deviations from the original specs (intentional, flagged)
- **Polars dropped** — `polars-stream 0.54.4` has an upstream compile bug; DuckDB (out-of-core) +
  native Rust ring buffers cover its role. Do not re-add polars without checking that bug.
- **Not microsecond HFT** — it's a fast *local analytics/staging* engine; tick→signal local
  processing is sub-ms, but the real latency floor is the broker network (~tens of ms).

## Known gaps / next steps (candidate work for the new session)
1. **Live Kite WebSocket (Phase 5)** — `ingestion_engine::run_live` + binary parser compile and are
   unit-tested against golden frames, but have **not been run against the live market**. Needs Kite
   creds (the plan: read the sibling `Intraday/kite_token.json` read-only) + market hours.
2. **DSR gate on scanner/finder** — the full reliability gauntlet runs only in the per-stock
   deep-dive; the Top-Scanner & Capital-Fit Finder still rank by a faster *pre-gate* confidence, so a
   name can read higher there (e.g. VEDL 89) than in its deep-dive (59). Optional: propagate the DSR
   gate to the light search for consistency.
4. **News engine** is built but OFF — add `NEWS_PROVIDER`+`NEWS_API_KEY` env to enable Marketaux/EODHD.
5. **Swing/staging/scanner endpoints** recompute on demand (~30–60s) — consider caching.

## How it was built
Incrementally, phase by phase, using **multi-agent workflows** (contract-skeleton → parallel agents
per file → integrate → verify) with byte-level cross-checks against an independent Python reference
for the stats. Each phase: `cargo build` + `cargo test` green + browser screenshot proof before commit.

## Git / push
```bash
git remote -v          # origin → the GitHub repo above (HTTPS, osxkeychain credential)
git push origin main   # already pushed; HEAD = this commit
```
