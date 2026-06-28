# RAM_ISTP — Session Handover

## ▶ RESUME (paste this into a new session)
> **Open `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`, read
> `SESSION_HANDOVER.md`, and continue on `main`.**

Then run the resume command:
```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
git checkout main && git pull origin main                   # all work is on main
pkill -f "ram_istp serve"; pkill -f "ram_istp live"         # stop any leftover instance (single-instance!)
cargo build && cargo test                                   # 153 tests should pass
./target/debug/ram_istp serve 30min                         # dashboards at http://127.0.0.1:8787
#   open http://127.0.0.1:8787/portfolio  → upload xlsx/CSV, or "Load my portfolio", or the ₹-horizon planner
```

## Latest session — Portfolio v2 (import + name resolver + capital horizon planner)
Built on the /portfolio + /desk dashboards; all display-only, honesty-first; **153 tests pass**. Verified end-to-end
against the owner's real 26-06-2026 Groww statement (12 holdings → ₹25,96,549, matches the statement exactly).
1. **Import overhaul** (`holdings_analytics::parse_csv`/`parse_holdings_bytes`/`header_map`/`find_col`/`sniff_delimiter`,
   `portfolio_import`): **PDF removed** (`pdf-extract` dep gone). A header-DETECTING CSV/TSV parser skips a statement's
   preamble rows and reads company name + ISIN + Quantity + Average buy price + Buy value + Closing price; derives avg
   cost from Buy value (exact, so totals reconcile); captures Closing price as the mark (off-archive names value
   correctly). Column matching is **exact-first then contains** (no loose-substring "Holding Period"→qty bugs). Paste
   box accepts tab- or comma-separated rows.
2. **Symbol resolver** (NEW `symbol_resolver.rs`): company-name → NSE symbol via `symbol_metadata.parquet` (normalized
   + abbreviation-expanded exact match, then a fuzzy token match that REFUSES to guess when the best two candidates are
   within a margin — e.g. "UNION BANK", bare "TATA" → unmatched-and-flagged, never the wrong stock). Fuzzy resolutions
   are surfaced as a "verify" warning. `HUDCO`, `TMCV` (post-demerger Tata Motors Ltd), etc. resolve cleanly.
3. **Merge + mark** (`holdings_analytics::merge_holdings`; `server::build_holdings_response` shared by `/api/holdings` +
   upload): duplicate symbols merged (sum qty, qty-weighted avg) BEFORE analyze — the root fix for the dup-symbol
   accounting findings. analyze prefers a statement close, else archive close, else cost.
4. **Capital horizon planner** (NEW `capital_planner.rs`; `GET /api/capital_plan?years=&capital=`; /portfolio UI
   section): scans the broad universe (`nse_daily_all.parquet` adjusted closes), horizon-weighted score (CAGR / RS-vs-
   NIFTY / trend / max-DD / full-history consistency / size) with liquidity + market-cap + consistency + ≥2y-history
   floors so no micro-cap pumps; CAGR winsorized in scoring, high-CAGR flagged ⚠; inverse-vol allocation with a proper
   **water-filling** 25%/name cap (≤2/sector, top-8) + greedy top-up to ~100% deploy; edge-map names tagged ✓. Cold
   ~12s, then **date-keyed cache → ~0.2s**. Framed strictly as HISTORICAL evidence (survivorship-bias disclosed),
   never a forecast/recommendation. Sample 10y: SOLARINDS/ICICIBANK/BAJFINANCE/TITAN/PIDILITE.
5. Prior correlation-ENB review fixes folded in (dup-symbol merge, /portfolio fallback no longer overclaims "overlap",
   /desk surfaces dropped names unconditionally, finite-guards). Two adversarial review passes run; **all confirmed
   findings (3 + 14) fixed and unit-tested.**
**Known follow-ups:** ISIN is captured but not used for matching (metadata `isin` col is unusable DOUBLE — name match
covers all real cases); planner survivorship bias is disclosed not corrected; a "download /portfolio as PDF" export is
still open.

---

A local, **signals-only** NSE intraday + swing + portfolio analytics platform in **Rust**,
querying a local Parquet archive via **DuckDB**, serving three web dashboards. It never places
broker orders — it stages signals for manual execution, tracks them synthetically, and shows the
user their own risk picture. **Honesty-first, real money: it surfaces evidence; the user decides.**

- **GitHub:** https://github.com/ramachandranbalasubrahmanian-svg/Ram-Most-Trusted-Trade-App
- **Local folder:** `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`
- **Base branch:** `main` · **Active branch (pushed):** `feat/latency-warmcache-and-robust-backtester`
- **State:** ~7,000+ LOC Rust + 4 HTML pages · **129 unit tests passing** · builds clean · **all work merged to `main`** (tip `15206ad`)

## How to resume in a new session
Open this folder. Then:
```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
git checkout feat/latency-warmcache-and-robust-backtester   # this session's work (pushed)
pkill -f "ram_istp serve"; pkill -f "ram_istp live"         # STOP any leftover instance first (see note ↓)
cargo build && cargo test                                   # 129 tests should pass
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
  journal portfolio analytics, manual journal, "Your Holdings — Risk Picture", and a **Rotation &
  Growth** block (trend/relative-strength keep/trim/rotate + edge-backed buy screen + rebalance).
- `/portfolio` — **NEW: Portfolio Review** (dedicated, layman-friendly): **upload PDF/Excel/CSV** or
  one-click **Load my portfolio** → summary, concentration, per-stock keep/trim/rotate verdicts,
  edge-backed uptrend buy ideas, a suggested reshuffle (cash/tax/before-after), growth scenarios.

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
| `holdings_analytics.rs` | real external-portfolio risk picture: HHI/heat/clusters/flags/edge-xref/Kelly + `my_portfolio()` preset (display-only, firewalled) |
| `portfolio_rotation.rs` | **NEW** rotation & growth: trend + relative-strength keep/trim/rotate, edge-backed uptrend buy screen, illustrative rebalance (LTCG + before/after), growth scenarios (display-only, firewalled) |
| `portfolio_import.rs` | upload ingest: Excel (`calamine`) + CSV/TSV → `HoldingInput` rows. **PDF removed.** Header-detecting parser lives in `holdings_analytics::parse_holdings_bytes` |
| `symbol_resolver.rs` | **NEW** company-name → NSE symbol via `symbol_metadata.parquet` (exact normalized + margin-guarded fuzzy; refuses ambiguous; firewalled display-only) |
| `capital_planner.rs` | **NEW** ₹-for-N-years horizon screen over `nse_daily_all.parquet`; horizon-weighted backtest-grounded score + floors + water-filling allocation; date-keyed cache; HISTORICAL evidence only, never a forecast |
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
| `server.rs` | Axum + `/ws/live_signals` + all `/api/*` (incl. `POST /api/holdings`, `GET /portfolio`, `POST /api/portfolio/upload` multipart) + `read_through` warm-cache |
| `main.rs` | tokio lifecycle: `init_tracing` → premarket → ingestion/analytics/risk threads → warm caches → server |
| `types.rs` | all data contracts (incl. Holding*/PortfolioAnalysis, RotationAnalysis/BuyCandidate/RebalancePlan, shortlist fields) |
| `ui/{index,intraday,desk,portfolio}.html` | the four Tailwind dashboards (served at request time — no rebuild for UI edits) |

## Latest work — Portfolio analytics (on `main`: commits `fc7d0d2` + `15206ad`)
1. **Rotation & Growth** (`portfolio_rotation.rs`; on `/desk` + `/api/holdings`): per-holding
   trend + relative-strength vs NIFTY → **Leader / Hold / Trim / Rotate-out** (Tata Motors demerger
   tickers held out as `Hold*`); an **edge-backed uptrend buy screen** (needs an eligible LONG edge
   AND beats NIFTY 6m+12m); an **illustrative rebalance** (rotate-out → redeploy, LTCG est,
   before/after risk profile); portfolio **growth scenario** ranges. Display-only, firewalled (imports
   only `types`/`config`/`storage_kernel`/`EdgeIndex`), no orders. +8 tests.
2. **Dedicated Portfolio page** (`/portfolio`, `ui/portfolio.html`, `portfolio_import.rs`):
   layman-friendly review — **upload PDF/Excel/CSV** (`POST /api/portfolio/upload` multipart) or
   one-click **Load my portfolio** (`my_portfolio()` = owner's real 13-stock book). Excel/CSV reliable
   (`calamine`); PDF best-effort (`pdf-extract`, warns to verify). +3 tests. Verified end-to-end.
   - **Known gaps / pick up next:** (a) "independent bets" here is **weight-based** (≈7.4), not the
     **correlation-based** ≈3.1 the old Python review showed — add a real return-correlation ENB +
     clusters; (b) **no manual/last-price** column, so off-archive names (TMLCV/TMPV) fall back to cost
     → total value reads low; (c) optional **"download as PDF"** export of the Portfolio page.

## What landed in the prior (latency/backtester) session — all on `main`
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
- **Portfolio follow-ups** (from the latest session): correlation-based ENB + clusters (match the ≈3.1
  figure); a manual/last-price column so demerger/off-archive names value correctly; "download as PDF"
  export of the `/portfolio` page; optionally fold MF holdings into the page.
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
**All work is on `main`** (and mirrored on the feature branch), pushed to GitHub. Latest tip: `15206ad`.
```bash
git status                       # clean (apart from untracked COMPETITIVE_ANALYSIS_2026.md, optional)
git log --oneline -8 origin/main # main == feature-branch tip == 15206ad
# how this session pushed to main (clean fast-forward — main was strictly behind):
git push origin HEAD:main
git push origin HEAD              # keep the feature branch in sync too
```
A dashboard instance may be running on :8787 (port + journal are single-instance — stop it before `serve`/`live` again).
