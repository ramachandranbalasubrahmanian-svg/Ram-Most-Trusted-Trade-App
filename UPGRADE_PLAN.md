# RAM_ISTP — Product Evaluation & Upgrade Plan

## 🇮🇳 Indian-market constraint conformance (session 2026-06-28, decisions taken)

Audited the 4-point NSE spec (timezone/hours, Kite instrument mapping, news API, parquet) against the
code. Implemented the safe, decision-free conformance items (signals-only, read-only/advisory; 100 tests):

- **Instrument-token mapping** (`kite_instruments.rs`, `ram_istp instruments`): public Kite dump →
  NSE-EQ filter → `tradingsymbol↔instrument_token` map, cached by IST date. Verified live: 9,903 NSE-EQ
  tokens; 541/541 archive symbols resolve. Cap = `LIVE_UNIVERSE_MAX` (1600, env-overridable). No string
  tickers; no secrets (public endpoint).
- **18 GB memory backstop**: `open_conn` sets DuckDB `memory_limit` (default 2 GB, env-overridable) +
  temp-dir spill → a pathological query spills to disk instead of OOM-crashing.
- **Market-hours gating primitives**: `config::is_regular_session` (09:15–15:30) + `is_premarket_gap_window`
  (09:00–09:08), unit-tested; the live tick path drops ticks outside the session (pre-open auction / post-close).
- **News budget guard**: `NewsBudget` (<`NEWS_DAILY_CAP`=90/day, resets per IST date) + `should_fetch`
  (Top-10 + volatility/VWAP-trigger gate), unit-tested. Marketaux `&countries=in&exchanges=NSE`; EODHD `.NSE`.
- **`.env.example`** committed (data root, DB memory, universe cap, news provider/key, Kite live creds).

**Decisions taken (owner delegated "best for the product"):**
- **Parquet path** → keep the shipped multi-timeframe `{DATA_ROOT}/{tf}/{SYMBOL}.parquet` as canonical
  (tested, matches disk, backtester uses all 9 timeframes). The spec's `/data/historical/NSE_{symbol}.parquet`
  single-1-min/25-yr path is the **stale artifact** — NOT migrating. Reality: 1-min ≈ 11 yr (2015→2026);
  25–30 yr only at daily. `RAM_ISTP_DATA_ROOT` overrides the root.
- **Live path** → built the **offline-verifiable safety pieces** now; the authenticated Kite socket
  (run_live wiring, Full-mode L2 subscription of the 1600 tokens, real OBI, news call-site) stays a
  creds-present / market-hours follow-up. The token-subscribe + 5+5 depth parser already exist and are tested.
- **Universe** → "all NSE-EQ ∩ local archive, capped at 1600" (no extra turnover feed needed).

**Non-negotiables held:** 15:15 stays an **ALERT** (the spec's "executes" wording is barred by signals-only);
replay OBI stays synthetic-labeled and out of gating; no `place/modify/cancel` anywhere; no secret logging.

---


## ✅ Implementation status (session 2026-06-28)
Done + verified (93 tests green; build clean at baseline 37 warnings):
- **Backtester P1** — `SimConfig`/`run_fill` refactor; `simulate`/`simulate_detailed` are thin
  legacy shims. **Proven byte-identical** to the original on real data (all 14,065 15min records)
  + golden anchor test `legacy_simconfig_matches_old_simulate`. New `AmbiguityPolicy`,
  `TradeOutcome`, `resolve_intrabar` scaffolding (intrabar path stubbed → P3).
- **Backtester P4** — scanner now applies the DSR gate (`gate_and_pick` over the 3-TF trial set)
  instead of scoring with neutral `dsr=1.0`; rows tagged `reliability:"scan"`. Test
  `scanner_gate_and_pick_applies_dsr_cap`. Closes the 89-in-scanner / 59-in-deep-dive gap.
- **Latency P7** — new `src/cache.rs`: `Cached<T>` (stale-while-revalidate + single-flight CAS),
  `KeyedCache<T>` on quantized `CapRiskKey`. 6 unit tests. std-only, no new deps.
- **Latency P8** — `AppState` uses the caches; `read_through` (no lock across `.await`);
  scanner/regime/swing/finder/staging handlers rewritten; staging reuses the finder cache.
  `RegimeInfo.built_ist` added + stamped.
- **Latency P9** — startup precompute (4 parallel warm threads before `serve`) + market-hours-gated
  scheduled refresh in the desk scheduler; circuit-breaker journal read cadence 3s→15s.
- **Latency P10** — `analyze_symbol` parallelized across `SUGGEST_INTERVALS` (`ANALYZE_CONN`
  thread-local) with deterministic merge. **Proven byte-identical** via `suggest` CLI; ~1.5×+ faster
  (best-of-3 ~2.5–3.2s vs ~4s; better in-server where worker conns persist).
- **Backtester P2** — same-bar ambiguity flag + slippage stress band (net expectancy at 1×/2×/3×
  slippage) on the deep-dive card (CLI + `intraday.html`); `costs::backtest_roundtrip_pct_scaled`.
  Display-only — never enters Confidence. Deep-dive core output **byte-identical** (band is additive).
- **Live runtime proof** — DONE: scanner/regime/swing/finder/staging warm-served in **~1–4ms** (was
  30–60s); deep-dive ~2.2s in-server; `built_ist` present; 8 concurrent scanner hits → one compute
  (single-flight); scanner rows gated to `reliability:"scan"`.

Remaining (not yet built):
- **Backtester P3** — real intrabar resolution + `--intrabar` flag + `EdgeMapFile` schema v2
  (scaffolding + `resolve_intrabar` stub already in `strategy_engine.rs`; opt-in, separate cache).
- **`DataQuality`** flags (the third P2 sub-item) — missing-bar / warm-up-NaN / low-sample score.
- **Tier-1 product features** — see §1 (VWAP-distance + S/R on cards, inline why-this/why-not,
  T2T/ASM/GSM + circuit tradability flags, live LTP/spread).

---


> Author lens: product owner + 20-yr NSE retail veteran. Goal: turn an honest *research bench*
> into a *trading desk a veteran can act on at 09:15*, with a genuinely robust backtester and
> sub-second page loads. **Honesty invariants are non-negotiable** (see bottom).

---

## 0. Regression anchor (this Rust project)

`63MOONS` is the *Python* project's anchor — **not present in this Rust archive**. This project's
regression reference is:

- **Cached edge maps** (must be byte-identical in legacy/pessimistic mode):
  - `cache/edge_map_15min.json` → `sha1 34d4659c…`
  - `cache/edge_map_30min.json` → `sha1 a337c222…`
- **Eyeball anchor edge:** `BAJFINANCE · gap_and_go · Short · 15min · n=130 · exp=0.1433565560483712 · PF=1.2659776591373888`
- **84 inline unit tests** green.

Any change in *legacy/pessimistic* mode that moves these → stop and surface it.

---

## 1. Product evaluation — the veteran's lens

**Verdict:** the most *intellectually honest* retail tool around — Confidence (is the edge real?)
separated from Conviction (is today good?), net-of-cost everywhere, DSR gate against overfitting,
no naked BUY button. That rigor is rare and must be protected. **But today it's a research bench,
not a desk** — a veteran can't act on it live yet. Gaps ranked by pain at the moment of decision:

### Tier 1 — can't trade live without these
1. **Live LTP + bid/ask spread, per name.** Entries currently show *last close*; at the open that's
   fiction (1–2% gaps routine). Need live quote + spread in paise before trusting an entry.
2. **Tradability flags — T2T / ASM / GSM / circuit bands.** #1 way retail loses a morning: size a
   setup, fire MIS, order *rejects* (ASM stage-2) or freezes at a circuit. Must red-flag *before* staging.
3. **VWAP distance + S/R on the intraday view.** NSE intraday lives on VWAP. "Entry +0.8% above VWAP
   into resistance 2,955" is the single most useful line we can add. Already computed internally — surface it.
4. **Live position MTM + 15:15 square-off as a running clock**, not a one-shot alert.

### Tier 2 — what makes it a *desk*
5. **Why-this / why-not, inline** (Conviction deltas on the scanner row, not hidden behind a click).
6. **Watchlists** beyond the fixed 15 — every veteran runs their own 30–40 names.
7. **Index/sector regime on the live page** (not a second tab).
8. **Portfolio heat / correlation** — 3 IT longs + 2 PSU-bank longs is 2 bets, not 5.
9. **A lightweight intraday chart** (candles + VWAP + the SL/target bands drawn).

### Tier 3 — nice
Kelly advisory · customizable alerts · CSV export · mobile layout · scale-out / trailing-SL in journal.

### Feature → endpoint roadmap (build order, lowest risk first)
| Feature | Touches | Notes |
|---|---|---|
| VWAP-distance + S/R on intraday/live cards | `suggestion_engine`, `analytics_kernel`, UI | data already computed; display-only |
| Inline why-this/why-not (Conviction deltas in rows) | `types.rs` (ScannerRow), UI | deltas already exist on deep-dive |
| Regime on live page | `server.rs` ws packet, UI | reuse cached regime (see latency) |
| Tradability flags (T2T/ASM/GSM/circuit) | new `tradability.rs`, Kite quote, UI | needs a daily surveillance list + live circuit limits |
| Live LTP/spread | `ingestion_engine` live client (Phase 5), Kite WS | gated on live creds + market hours |
| Watchlists | `config`/new store, UI, all scans | per-user symbol set |
| Portfolio heat/correlation | `portfolio_analytics`, UI | needs sector map (partly present) |

> This round implements the **two engineering pillars** (§2, §3). The feature roadmap above is
> specified for follow-on rounds.

---

## 2. Robust backtester (this round)

Today: `simulate()` (strategy_engine.rs:646-722) and `simulate_detailed()` (728-804) fill on **bar
OHLC only**; on a bar whose range spans both SL and target, **SL is checked first** (assumption, not
data); one fixed slippage (costs.rs, 0.025% each-way); exit at last-bar close if neither hits intraday.

**Soft spots fixed:** (a) same-bar SL/target ambiguity resolved by assumption; (b) no slippage stress
view; (c) no data-quality gate; (d) scanner ranks on a *pre-gate* confidence so a name reads 89 in the
scanner but 59 in its deep-dive.

### Design — one fill core, everything defaults to legacy
Introduce `SimConfig` + a single `run_fill()` that both `simulate`/`simulate_detailed` delegate to as
thin shims. **`SimConfig::legacy()` reproduces today's model byte-for-byte** (slippage_mult=1.0,
`AmbiguityPolicy::PessimisticStopFirst`, no finer data) → anchor safe.

```rust
enum AmbiguityPolicy { PessimisticStopFirst /*default=legacy*/, IntrabarResolved }
struct SimConfig<'a> { k, rr, cost, slippage_mult, ambiguity, finer: Option<&'a [Candle]> }
struct TradeOutcome { entry_idx, exit_idx, r, ambiguous: bool, intrabar_resolved: bool }
fn run_fill(bars, atr, entries, dir, cfg) -> Vec<TradeOutcome>   // the only fill logic
pub fn simulate(..7 args..) -> Vec<f64>            // shim → run_fill(legacy)
pub fn simulate_detailed(..7 args..) -> Vec<(usize,f64)>  // shim → run_fill(legacy)
pub fn simulate_full(bars,atr,entries,dir,cfg) -> Vec<TradeOutcome>  // new
```

**P1 — refactor + guard.** Extract `run_fill`; reduce the two fns to shims; add golden test
`legacy_simconfig_matches_old_simulate` (feed existing fixtures, assert byte-equal R). The existing
`simulate_hits_target_then_stop` / `metrics_basic` tests must pass unchanged.

**P2 — honesty stats (display-only, never feed Confidence).**
- *Ambiguity flag:* on a both-touched bar set `ambiguous=true`; pessimistic default still takes SL.
  New `Metrics.ambiguous_frac` (`#[serde(default)]`).
- *Slippage band:* `slippage_band(...)` re-runs `run_fill` at 1×/2×/3× → `SlippageBand{exp_1x..exp_3x, pf_*}`,
  surfaced on the **deep-dive card only** (not every edge-map row → keeps the 5.7 MB cache from tripling).
  `costs::backtest_roundtrip_pct_scaled(mult)`; at 1× equals `backtest_roundtrip_pct()` exactly.
- *Data quality:* `assess_quality(bars, atr, tf) -> DataQuality{ missing_bar_frac, warmup_nan_frac,
  low_sample, score, flags }`. Display-only honesty metadata.

**P3 — intrabar resolution (opt-in) + cache versioning.**
- `AmbiguityPolicy::IntrabarResolved` drops to next-finer tf (`Timeframe::finer()`, down to Minute) for
  the ambiguous bar; `resolve_intrabar(finer, day, dir, sl, tgt) -> Option<f64>`; fall back to
  pessimistic when finer data can't decide. Loader reuses `storage_kernel::load_candles`.
- Re-baselines R → **separate cache file**: `edge_map_{tf}_intrabar.json` (pessimistic filename
  unchanged → old caches still load). New `--intrabar` CLI flag (main.rs arg parse); default OFF.
- `EdgeMapFile{ schema:u32=2, fill_mode, records }`; `load_edge_map` tries v2 then falls back to the
  old bare `Vec<EdgeRecord>` (schema 1). New fields all `#[serde(default)]`.

**P4 — scanner/deep-dive DSR reconciliation.** Root cause: `build_config_stat` hard-codes `dsr=1.0`;
the deep-dive overwrites it via `deflated_sharpe(...)` before the gate, the scanner doesn't.
Fix: in `scan_symbol`, capture `trial_sharpes` from the configs it already runs (15m/30m/60m), apply
`deflated_sharpe` + `ensure_confidence` in a second pass. Scanner trial set ⊂ deep-dive's → scanner can
never read *higher* than the deep-dive. Cost: one `deflated_sharpe` per candidate (µs) — **no new
loads, no +60s**. Optional `reliability:"scan"|"deep"` tag on `ScannerRow` for an honest UI footnote.

### Tests
`legacy_simconfig_matches_old_simulate` (anchor guard) · `ambiguous_bar_flagged_and_pessimistic` ·
`intrabar_resolution_picks_target_first` (+stop-first variant) · `slippage_band_1x_equals_baseline`
(+monotone) · `quality_flags_missing_and_warmup` · `edge_map_schema_back_compat` ·
`scanner_and_deepdive_dsr_reconcile`.

---

## 3. Latency pass (this round)

Slow endpoints today (recompute on demand, loop the ~1500-symbol universe):
`/api/scanner` 30–60s (cached once, never refreshed) · `/api/finder` 30–60s (no cache) ·
`/api/regime` 10–20s · `/api/swing` 10–20s · `/api/staging` 30–60s (calls finder inline) ·
`/api/suggest/{symbol}` 5–15s (sequential over 240 configs).

### Design — warm caches, stale-while-revalidate, single-flight
**New `src/cache.rs` (std-only, no new deps):**
```rust
struct Cached<T>   { inner: RwLock<Option<Entry<T>>>, refreshing: AtomicBool, ttl }
struct Entry<T>    { value: T, built_at: Instant, built_ist: String }   // freshness on every value
impl Cached<T> { lookup()->Lookup<T>; try_begin_refresh()->bool /*CAS, single-flight*/; store(..); abort_refresh(); }
struct CapRiskKey  { cap_bucket: u64 /*₹1k*/, risk_bp: u32 /*1bp*/ }     // quantized
struct KeyedCache<T>{ map: RwLock<HashMap<CapRiskKey, Arc<Cached<T>>>>, ttl, cap }
```
- **Read-through helper** in `server.rs`: hit→serve; stale→serve stale **and** detach a background
  refresh (only if `try_begin_refresh` wins); cold-miss→compute inline once (eliminated by startup warm).
  **No lock ever held across `.await`** (clone out of the guard, like the existing scanner handler 194-198).
- **Caches:** scanner/regime/swing = `Cached<T>`; finder = `KeyedCache<FinderResult>` on (capital,risk).
  TTLs ~2–5 min. Add `built_ist` to `RegimeInfo` (types.rs:673) so every cached payload is timestamped.
- **Staging reuses the finder cache** at `(CAPITAL_POOL, tier.pct())` — 3 keys shared with the finder page.

**Startup precompute (main.rs, before `rt.block_on`):** 4 detached `std::thread`s warm
scanner+regime+swing+default-key finder in parallel (≈ slowest single scan ~60s, not the serial sum).
Gated by `try_begin_refresh()` so a mid-warm request doesn't double-compute.

**Scheduled refresh:** extend the existing 3s scheduler loop (main.rs:407-455) with a market-hours
gate (`config::premarket_start()..session_close()`); poll staleness, kick at most one refresh per cache
per TTL. No overnight CPU burn. Reduce the breaker's journal-read cadence 3s→15s (removes ~80% of
journal lock acquisitions; export still second-grained around 15:45).

**Parallelize the deep-dive:** `analyze_symbol` (suggestion_engine.rs:807-853) is sequential over 6
intervals. Parallelize with `par_iter` + `ANALYZE_CONN` thread-local DuckDB (mirror SCAN_CONN);
`compute_indicators` already hoisted per-interval. Read the NIFTY regime map **once before** the
parallel region. Merge partials in `SUGGEST_INTERVALS` order → **byte-identical** output. Expect
5–15s → ~1–3s.

**Journal:** keep the single `Arc<Mutex<Connection>>` (manual, human-paced writes; durability >
throughput we don't need). Future path if automated logging arrives: dedicated writer thread fed by the
existing `crossbeam-channel`.

### Verification
- Per-endpoint timing via `tracing::info!("api=.. cache=hit|stale|miss compute_ms=..")` (reuse the
  `tick_to_signal_us` `Instant` pattern).
- Black-box (user runs): `for ep in scanner regime swing "finder?capital=1000000&risk=1.0" "staging?risk=Moderate" "suggest/RELIANCE"; do curl -s -o /dev/null -w "%{time_total}s /api/$ep\n" "http://127.0.0.1:8787/api/$ep"; done` cold vs warm.
- **Targets:** warm scanner/finder/regime/swing < 50ms · suggest 5–15s → 1–3s · staging instant once Moderate key warm.
- **Single-flight proof:** 10 concurrent `curl /api/scanner` at startup → exactly one `compute_ms` line.
- **Freshness:** every cached JSON carries non-empty `built_ist`; UI renders "built <IST>".
- **Anchor:** `cargo test` green; cache checksums unchanged in legacy mode; deep-dive numbers byte-identical.

---

## 4. Honesty invariants (KEEP)
1. **Signals only.** Nothing places/modifies/cancels a broker order. Journal is synthetic.
2. **Net-of-cost truth.** All R/P&L net of itemized costs. Past edge ≠ future return — scenarios, never promises.
3. **Confidence = t-stat + behavioral penalties + DSR gate ONLY.** `DataQuality`/`SlippageBand`/`ambiguous_frac`
   are display-only and never enter `ConfInput`. Confidence and Conviction never merge.
4. **15:15 square-off is an ALERT, not an order.**
5. **Cached values are clearly timestamped** (`built_ist`) and never presented as live.

## 5. Build order (this round)
P1 backtester refactor+guard → P7 cache.rs → P2 honesty stats → P4 scanner DSR → P8 server wiring →
P9 startup/scheduler → P10 deep-dive parallelize → P3 intrabar+versioning (last; opt-in, separate cache).
`cargo build` + `cargo test` green after **every** phase; anchor checked after P1, P4, P10.
