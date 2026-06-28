# RAM_ISTP — Session Handover

## ▶ RESUME (paste this into a new session)
> **Open `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`, read
> `SESSION_HANDOVER.md`, and continue on branch `feat/latency-warmcache-and-robust-backtester`.**

Then run the resume command:
```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
git checkout feat/latency-warmcache-and-robust-backtester   # this session's work (pushed)
pkill -f "ram_istp serve"; pkill -f "ram_istp live"         # stop any leftover instance (single-instance!)
cargo build && cargo test                                   # 118 tests should pass
./target/debug/ram_istp serve 30min                         # dashboards at http://127.0.0.1:8787
```

---

A local, **signals-only** NSE intraday + swing + portfolio analytics platform in **Rust**,
querying a local Parquet archive via **DuckDB**, serving three web dashboards. It never places
broker orders — it stages signals for manual execution, tracks them synthetically, and shows the
user their own risk picture. **Honesty-first, real money: it surfaces evidence; the user decides.**

- **GitHub:** https://github.com/ramachandranbalasubrahmanian-svg/Ram-Most-Trusted-Trade-App
- **Local folder:** `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`
- **Base branch:** `main` · **Active branch (pushed):** `feat/latency-warmcache-and-robust-backtester`
- **State:** ~6,500+ LOC Rust + 3 HTML pages · **118 unit tests passing** · builds clean

## How to resume in a new session
Open this folder. Then:
```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
git checkout feat/latency-warmcache-and-robust-backtester   # this session's work (pushed)
pkill -f "ram_istp serve"; pkill -f "ram_istp live"         # STOP any leftover instance first (see note ↓)
cargo build && cargo test                                   # 118 tests should pass
./target/debug/ram_istp serve 30min                         # dashboards at http://127.0.0.1:8787
```

> ⚠️ **Single-instance: stop the old server before starting a new one.** The port `:8787` AND the
> journal DuckDB file are both single-instance — a leftover `serve`/`live` process from a prior
> session will make a new one fail with a port-bind error or `Conflicting lock ... journal_2026.duckdb`.
> Always `pkill -f "ram_istp serve"; pkill -f "ram_istp live"` first (the `pkill` step above), confirm
> with `pgrep -fl ram_istp`, then launch. `serve` (replay) and `live` (Kite) cannot run at the same
> time — they share the port and journal.
Read `UPGRADE_PLAN.md` (engineering history + live go-live checklist) and `PRODUCT_ROADMAP.md`
(competitive analysis + feature catalog + what's next) for full context.

> The **14 GB data archive is NOT in git** (gitignored). It lives at
> `RAM_ISTP_Rust_Architecture/1500-Stocks-Parquest/` (`minute/ 5min/ 15min/ 30min/ 60min/ 1day/ …`,
> files `{SYMBOL}.parquet`; ~541 symbols with minute data). 1-min spans ~11 yr (2015→2026);
> ~25–30 yr only at daily. A fresh clone elsewhere has NO data — keep this folder, or point
> `RAM_ISTP_DATA_ROOT` at the archive.

## CLI commands
```bash
ram_istp premarket [SYMBOL...]      # pre-market scan: macro ATR/52w S-R + VAH/VAL
ram_istp backtest [TF] [SYMBOL...]  # backtest strategy library → cached edge map (cache/)
ram_istp serve [TF]                 # replay + all dashboards (default 30min)
ram_istp suggest SYMBOL             # per-stock intraday deep-dive (CLI view of /intraday)
ram_istp instruments                # NEW: refresh NSE tradingsymbol→instrument_token map (public dump)
ram_istp live [TF]                  # NEW: LIVE Kite feed + dashboards (needs creds in .env + market hours)
```
Tracing now works: prefix with `RUST_LOG=info` to see connect/WS/refresh logs.

## Web pages (serve/live on :8787)
- `/`          — **Live Signals**: Top-10 Buy/Sell, budget/risk sliders, risk meter, µs diagnostics.
- `/intraday`  — **Intraday Suggestion**: per-stock 4-strategy deep-dive (Confidence/Conviction/DSR/MC/
  bootstrap + slippage stress band + **★ high-conviction shortlist badge**), 10-Buy/10-Sell scanner
  (★ shortlist marker), Capital-Fit ATR Finder.
- `/desk`      — **Trading Desk**: signal-freeze + circuit breaker, staging console, swing ledger,
  journal portfolio analytics, manual journal, and **NEW: "Your Holdings — Risk Picture"** (paste
  CSV / Load sample → concentration, sector/broker heat, clusters, per-name flags, advisory Kelly).

## Architecture (src/)
| File | Role |
|------|------|
| `config.rs` | paths, IST sessions + `is_regular_session`/`is_premarket_gap_window`, budget/risk, capital, `LIVE_UNIVERSE_MAX`, `NEWS_DAILY_CAP`, `SHORTLIST_*` knobs, DB memory env |
| `cache.rs` | **NEW** stale-while-revalidate `Cached<T>` (single-flight) + `KeyedCache<T>` for the warm-cache layer |
| `storage_kernel.rs` | DuckDB out-of-core parquet reads (+ `memory_limit`/spill backstop); pre-market scan; candle loaders |
| `strategy_engine.rs` | 13-strategy library + fill core `run_fill`/`SimConfig` (legacy-identical) + `AmbiguityPolicy` + edge map |
| `stats.rs` | t-stat, Sharpe, Wilson, MC, bootstrap CI, **DSR**, Confidence, Conviction, `is_high_conviction_shortlist` |
| `validation.rs` | purged+embargoed OOS, walk-forward, parameter robustness |
| `regime.rs` | NIFTY up/down regime map + consistency |
| `suggestion_engine.rs` | per-stock deep-dive (parallelized across intervals) + scanner (DSR-gated) + Capital-Fit + regime |
| `holdings_analytics.rs` | **NEW** real external-portfolio risk picture: HHI/heat/clusters/flags/edge-xref/Kelly (display-only, firewalled) |
| `kite_instruments.rs` | **NEW** public Kite instruments dump → NSE symbol↔token map, cached by IST date |
| `costs.rs` | itemized Indian intraday charges + `backtest_roundtrip_pct_scaled` (slippage band) |
| `risk_manager.rs` | sizing + projected P&L + risk meter + 15:15 square-off ALERT |
| `analytics_kernel.rs` | per-symbol ring buffers, OBI/VWAP/z-score/RVOL, ranking |
| `ingestion_engine.rs` | replay simulator + Kite WS Full-mode parser + live client (`run_live`/`run_live_blocking`, IST-gated) |
| `execution_staging.rs` | SEBI-compliant LIMIT Bracket-Order staging (copy/paste only) |
| `circuit_breaker.rs` | synthetic MTM → Signal Freeze at −2% pool |
| `journal_sync.rs` | DuckDB manual-validation journal + slippage/PnL + 15:45 CSV export |
| `portfolio_analytics.rs` | journal-based (CLOSED-trade) analytics — distinct from `holdings_analytics` |
| `news_engine.rs` | Marketaux/EODHD sentiment (OFF by default) + `NewsBudget`/`should_fetch` (<100/day, Top-10 trigger) |
| `server.rs` | Axum + `/ws/live_signals` + all `/api/*` (incl. `POST /api/holdings`) + `read_through` warm-cache |
| `main.rs` | tokio lifecycle: `init_tracing` → premarket → ingestion/analytics/risk threads → warm caches → server |
| `types.rs` | all data contracts (incl. Holding*/PortfolioAnalysis, shortlist fields) |
| `ui/{index,intraday,desk}.html` | the three Tailwind dashboards (served at request time — no rebuild for UI edits) |

## What landed THIS session (all on the pushed branch)
1. **Latency** — `cache.rs` warm caches + startup precompute + market-hours scheduled refresh: scanner/
   regime/swing/finder/staging **30–60s → ~1–4ms** (proven live). Deep-dive parallelized ~4s→~2.2s (byte-identical).
2. **Robust backtester** — `run_fill`/`SimConfig` (legacy byte-identical), same-bar **ambiguity flag**,
   **slippage stress band** (1×/2×/3× on the deep-dive card), **scanner↔deep-dive DSR reconciliation**.
3. **NSE conformance** — `kite_instruments` token mapping (verified live: 9,903 NSE-EQ tokens), DuckDB
   18GB memory backstop, IST market-hours gating primitives, news budget guard, `.env.example`.
4. **Live Kite feed** — `ram_istp live` wired (mirrors serve, Full-mode token subscribe). **Found+fixed a
   shipped TLS bug** (native-tls) + added tracing init + hardened `data_root` against empty env. Verified:
   TLS+subscribe handshake OK; HTTP 400 = the access token must be re-minted each morning.
5. **Holdings risk picture** (`holdings_analytics`, `POST /api/holdings`, Desk tab) — display-only, no advice.
6. **High-conviction shortlist** (`stats::is_high_conviction_shortlist`) — Confidence≥70 + Wilson floor≥60% +
   DSR gate; badge + fixed "not a sure shot" caveat. Honestly flags 0 today (nothing clears the bar).

## Going LIVE (each trading morning)
1. Re-authenticate with Kite → fresh `access_token` (tokens expire ~6 AM IST).
2. Update `KITE_ACCESS_TOKEN=` in `.env` (gitignored; never commit it). `KITE_API_KEY` already set.
3. During 09:15–15:30 IST: `RUST_LOG=info ./target/debug/ram_istp live 30min` → watch for
   `kite ws: connected; subscribing instruments=N`. Dashboard at :8787 (mode="live").
4. If a *fresh* token still returns HTTP 400, it's a handshake detail to debug (header/endpoint), not the token.

## Next-up roadmap (specced in PRODUCT_ROADMAP.md, not yet built)
- **C1 why-this/why-not** inline (surface the dropped `build_confidence` penalties + Conviction deltas).
- **C2 regime-conditional display** (`analyze_symbol` already splits up/down NIFTY R-arrays then discards them).
- **C3 correlation-aware exposure** on live Top-10 (info Alert; never prunes/sizes).
- **D1 live LTP + bid/ask spread + tradability flags (T2T/ASM/GSM/circuit)** on cards — top "can't trade live without it" gap.
- **D3 aggregate portfolio risk-of-ruin** (reuse the seeded MC engine on the real holdings book).
- **Backtester P3 intrabar resolution** (`--intrabar`, versioned cache; scaffolding + stub already in `strategy_engine`).

## NON-NEGOTIABLE honesty invariants (KEEP)
1. **Signals only** — nothing places/modifies/cancels a broker order. No personalized advice (not a licensed advisor).
2. **No naked BUY/SELL, no "sure shot"** — edges + statistics + caveats; the shortlist is caveated, never a certainty.
3. **Confidence = t-stat + behavioral penalties + DSR gate ONLY.** Conviction/regime/microstructure/holdings/
   shortlist are **display-only** and never gate or inflate it. `holdings_analytics` imports only `types`+`EdgeIndex`.
4. **Net-of-cost truth**; **15:15 square-off is an ALERT**; **cached values carry `built_ist`** (never stale-as-live).
5. **Never print/log** `KITE_API_KEY`/`KITE_ACCESS_TOKEN`/`NEWS_API_KEY`. Credentials live in `.env` only.
6. **Regression anchor** (deep-dive must stay byte-identical): `63MOONS · VWAP · SELL · 15m · +0.494R · PF 2.01 · n=51 · conf=72`.

## Git
```bash
git status                       # working tree clean on the feature branch
git log --oneline -8             # this session's commits
git push origin feat/latency-warmcache-and-robust-backtester   # already pushed; open a PR on GitHub
```
A dashboard instance may be running on :8787 (port + journal are single-instance — stop it before `serve`/`live` again).
