# RAM_ISTP — Session Handover

## ▶ RESUME (paste this into a new session)
> **Open `/Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture`, read
> `SESSION_HANDOVER.md` (esp. ◀ THIS SESSION + the one remaining P1 item below), and continue on `main`.**
>
> **Git state (2026-06-29 cont.):** clean working tree, **9 commits AHEAD of origin and NOT pushed** — the owner
> chose to HOLD the work local for review. Local `main` tip = **`4bd783e`**; `origin/main` still = **`82b8cf2`**.
> **Decide on `git push origin main` before doing more.** **189 tests pass; build clean; both anchors byte-identical.**
> **ALL of P0-2b + the split guard + ALL FOUR P1 items + an IndianAPI news-caution layer are DONE this session.**
>
> **News layer (`4bd783e`):** `GET /api/news?symbol=&side=` + a per-pick caution row under the ★ Sharpest setups —
> flags CAUTIOUS BUY/SELL when news/today's move contradicts the signal. **Needs `INDIANAPI_KEY` in `.env` to activate**
> (a credential — the owner adds it; app reads via dotenvy, never logs). Without the key it shows an honest "unavailable".
> The 14 GB archive + `cache/` edge maps + `fundamentals.parquet` are gitignored (local only). (Only untracked file:
> `COMPETITIVE_ANALYSIS_2026.md`, pre-existing.)
>
> The 7 local commits (oldest→newest): `d4b74f8` P0-2b details onboarding · `fc6f454` data-quality guard ·
> `5af6c01` fundamentals panel · `815b6dd` shrunk-expectancy ranking · `76ec8f9` CPCV/PBO panel · `018dac4` docs ·
> `cc2bfa5` edge-map robustness columns. See ◀ THIS SESSION.
> **⚠ Edge-map rebuild state:** `cc2bfa5` rebuilt **only `edge_map_30min`** (the live tf) to populate robustness.
> **`5min/15min/60min` still carry default (empty) robustness until you run `ram_istp backtest <tf>`** for each.
> **Resume shell:**
> ```bash
> . "$HOME/.cargo/env"; cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
> git log --oneline -8                                  # tip cc2bfa5; origin behind at 82b8cf2 (NOT pushed)
> pkill -f "ram_istp serve"; pkill -f "ram_istp live"   # single-instance: stop leftovers first
> cargo build && cargo test                             # 182 tests pass
> ./target/debug/ram_istp serve 30min                   # dashboards → :8787 (finder/scanner warm ~2 min, in bg)
> ```

Then run the resume command:
```bash
. "$HOME/.cargo/env"
cd /Users/srihariramachandran/Documents/Claude-Projects/RAM_ISTP_Rust_Architecture
git log --oneline -6                                        # tip dc0913c; origin == local (all pushed)
pkill -f "ram_istp serve"; pkill -f "ram_istp live"         # stop any leftover instance (single-instance!)
cargo build && cargo test                                   # 167 tests should pass
./target/debug/ram_istp serve 30min                         # dashboards at http://127.0.0.1:8787
#   NOTE: finder/scanner universe warms ~2 min in the background after start (first run slow, then instant)
#   /            → Live Signals: Top-10 + EDGE-MAP FRESHNESS banner + tradability badges (⛔NO-MIS/THIN/₹LOW/green ✓)
#                  + "★ Sharpest setups" (top-2 Buy/2 Sell, stock·qty·profit·loss; blocked names hard-excluded)
#   /intraday    → deep-dive (RED ⛔ STAY-AWAY banner if T2T/surveillance) · Scanner (now ATR·SL·Target·Qty·Profit·Loss)
#                  · Capital-Fit Finder with a 3rd input: "Max ATR / share" slider (₹1→today's max), green=clean symbol
#   /add_stock   → add an NSE code → downloads candles → AUTO-ONBOARDS (backtest+merge) → shows eligible edges
#   /api/edge_map_status · /api/onboard_symbol · /api/tradability (verdict: blocked|high_risk|caution|ok)
# THEN: remaining P0 — P0-2b add_stock DETAILS onboarding (symbol_metadata/sector/corp-actions/fundamentals via Python),
#       then P1 (robustness columns, shrunk ranking, CPCV/PBO, fundamentals, coverage panel).
```

## ◀ THIS SESSION (2026-06-29 cont.) — P0-2b + split guard + ALL 4 P1 items (7 commits, LOCAL ONLY, not pushed)
All display-only / signals-only / firewalled; eligibility gate, Confidence, cost model, and **both anchors untouched**
(`anchor_bajfinance_edge_map_stable` + `anchor_63moons_deep_dive_stable` byte-identical). **182 tests** (was 167).
**6th feature commit `cc2bfa5` = edge-map robustness columns (the 4th P1 item) — see the ✅ DONE block below.**
Every feature was verified end-to-end (live endpoint + rendered-output checks), not just unit-tested.
1. **P0-2b add_stock DETAILS onboarding** (`d4b74f8`) — NEW `enrich_stock.py` (sibling of `download_stock.py`): upserts
   `symbol_metadata` (sector/industry/mcap/name/isin from Yahoo `.info`, recent_listings fallback), upserts corp-actions
   (per-stock + combined parquet, replacing ONLY that symbol's rows), writes the split-adjusted `daily_adj` slice, and
   pulls indianapi fundamentals **only if `INDIANAPI_KEY` is set** (paid). Atomic writes; honest "—" on missing fields.
   `POST /api/enrich_symbol` (mirrors add_stock_handler; strict symbol whitelist) + chained in `add_stock.html`
   (download → onboard edges → enrich details). Upsert proven non-destructive (all 21,016 other corp-action rows
   byte-identical by hash).
2. **Split-continuity / bad-tick data-quality guard** (`fc6f454`) — NEW `src/data_quality.rs` (firewalled: config +
   storage_kernel). Pure `assess()`: invalid/non-finite prices, worst single-day discontinuity, uncorrected-split
   cross-ref vs `corporate_actions_all.parquet`, + a "recent corporate action" tag. Verdict ok|caution|unreliable.
   `GET /api/data_quality?symbol=` (on-demand, live tf) → red "DATA QUALITY — UNRELIABLE" banner on the deep-dive.
   **Calibrated on real data, NO hardcoded name list:** PRIVISCL (65 ₹0.00 bars, 3689× jump) + KAMOPAINTS (378× jump,
   ₹0.02) → unreliable; **CUPID & RELIANCE → ok (no false positive — CUPID's splits ARE adjusted, worst jump 1.25×)**,
   CUPID still surfaces its 5:1/2:1 split as context. +6 tests.
3. **P1 fundamentals panel** (`5af6c01`) — NEW `build_fundamentals.py` flattens the unconsumed `indianapi/stock/*.json`
   → `fundamentals.parquet` (P/E, ROE, D/E, PEG, 5y growth, margin, div yield, P/B, promoter %, mcap; 270 covered, 6
   empty skipped; spot-checked accurate). NEW `src/fundamentals.rs` (firewalled: storage_kernel) +
   `GET /api/fundamentals?symbol=` → a "Fundamentals · context only" grid on the deep-dive (honest "—"/uncovered).
   `enrich_stock.py` does a best-effort single-symbol upsert (reuses `flatten_one`). +2 tests.
4. **P1 shrunk-expectancy ranking** (`815b6dd`) — the live Top-10 ranked on RAW expectancy × live_score, so small-n
   flukes topped it (67 eligible edges carry n<50). Now ranks on James–Stein-shrunk expectancy (`SHRINK_PRIOR_R=0.0`,
   `SHRINK_STRENGTH=40` in config). `analytics_kernel::snapshot_candidates`: `score = shrunk × live_score`; raw carried
   for display. `Candidate`/`RankedSignal` gain `shrunk_expectancy_r`; `index.html` shows a "ranks X.XXR" tag + rewritten
   Score help. **Anchor-safe (anchors test EdgeRecord.metrics, not Top-10 order).** Verified live: STUDDS (n=65) now sits
   below MWL (n=215) despite identical raw 0.264R. +2 tests.
5. **P1 CPCV / PBO panel** (`76ec8f9`) — NEW pure `src/cpcv.rs` (Bailey CSCV → Probability of Backtest Overfitting;
   block_means + cscv_pbo with midrank ties + C(S,S/2) splits; +5 tests incl. dominant→PBO≈0, specialist→PBO>60%).
   `suggestion_engine`: a READ-ONLY tap at the per-interval merge computes PBO **per timeframe** (configs must share one
   bar grid) from the trades already backtested, 8 blocks. `StockSuggestion.pbo_by_tf` (#[serde(default)]) →
   per-tf PBO panel on the deep-dive. **Anchor-safe (tap only reads the already-built config set).** Verified live on
   RELIANCE (2.4s): 40 configs × 8 blocks/tf, PBO 0–4.3%.

### ✅ DONE — robustness columns on the edge-map / Top-10 tier (`cc2bfa5`)
The 4th P1 item is now DONE. `EdgeRecord` + `EligibleEdge`/`Candidate`/`RankedSignal` gained a `Robustness`
sub-struct ({oos_expectancy, oos_n, wf_consistency, dsr}, all `#[serde(default)]`). `backtest_symbol` computes it from
the SAME trades as `metrics` (switched to `simulate_detailed`, which is byte-identical to `simulate` — both via
`run_fill`/`SimConfig::legacy` — so metrics + the anchor are untouched) via `validation::purged_embargoed_split`
(0.30/0.02) + `walkforward_consistency` (5 folds) + a per-symbol DSR over its ~26 strat×dir trial Sharpes. Threaded
through `build_index`→`snapshot_candidates`→`to_ranked`; `build_note` flags OOS≤0 / WF<50%; `index.html` shows a per-row
OOS·WF·DSR annotation. `eligible()` UNCHANGED. Anchor green + a new populated-and-in-range test. Verified live: OMNI
(n=54) flags "OOS −0.06R (fails out-of-sample)". **Caveat:** robustness shows only after a `backtest <tf>` rebuild —
**done for 30min; rebuild 5/15/60min when wanted.** DSR is near-zero for most edge-map edges (deflated vs 26 siblings) —
shown in the row but deliberately NOT a Note flag (would be noise).

### ▶▶ Possible NEXT items (none started; pick per priority)
- **Rebuild `5min/15min/60min` edge maps** so robustness shows on those tfs too (`ram_istp backtest 15min`, etc.).
- **Spec's "show CIs" on the Top-10** — needs the edge map to store per-trade R-series (it stores summary stats only;
  `expectancy_ci` needs the series). The deep-dive already shows CIs. A format change; opt-in + separate cache if done.
- The earlier-flagged number-movers (cost-constant reconciliation, DSR effective-trials→12) — still require explicit
  sign-off + an anchor re-baseline before flipping. Untouched this session.

## ◀ EARLIER 2026-06-29 — intraday safety gate + perf + ATR input (all pushed: `141dee0`, `337f8cc`, `dc0913c`)
All display-only / signals-only; Confidence + edge map + the 63MOONS/BAJFINANCE anchors untouched. **167 tests** (was 165).
1. **Finder perf** (`141dee0`) — `suggestion_engine::fit_universe()` caches the capital/risk-INDEPENDENT backtest search
   (date|count keyed); capital/risk slider changes now re-run only the cheap sizing loop. **~81s → ~15ms**, results
   byte-identical. Startup warm builds it once (~2 min).
2. **SEBI-surveillance / intraday gate** (`337f8cc`) — `tradability.rs` now returns a **verdict**: `blocked` (T2T/BE/BZ
   series OR a loaded surveillance name) / `high_risk` (very-thin liquidity) / `caution` (thin/penny/micro) / `ok`.
   Enforced on EVERY intraday rec surface via `/api/tradability`: `/intraday` deep-dive shows a prominent **RED "⛔ STAY
   AWAY"** banner above the stats when blocked (fires even if all params are green); scanner+finder+Top-10 get ⛔/⚠ badges;
   `/` **Best-Picks** ("★ Sharpest setups" minimal top-2 Buy/Sell panel) HARD-EXCLUDES blocked. NOT applied to
   /desk + /portfolio (positional/delivery, where T2T is allowed). **ASM/GSM enable hook:** drop a
   `surveillance.csv` (`symbol,measure`) in the data root → those names become `blocked` (proven; absent ⇒ honest
   "not loaded — verify on NSE", never assumed clean).
3. **Scanner trade-plan + Max-ATR 3rd input + green signal** (`dc0913c`) —
   - Scanner (10 Buy/10 Sell) now shows **ATR · SL · Target · Qty · Profit · Loss**, sized to capital+risk, **net of cost**
     (handler sizes the cached scan per-request via `size_scan_result`; `ScannerRow` carries sl/target/atr + sizing).
   - **Max ATR / share** slider (₹1 → `FinderResult.max_atr_universe`, ₹386 now) — a 3rd input next to Capital+Risk.
     Finder filters rows by ATR ≤ ceiling **per-request in the handler** (KeyedCache stays valid); scanner filters
     client-side. "Best stocks for Capital + Risk + ATR." Lower ATR ⇒ tighter ₹ stop ⇒ more shares.
   - **Green "clean for intraday"** signal: verdict==ok → finder symbol GREEN + ✓ badge on Top-10/scanner (amber=caution/
     high-risk, red=blocked); side colours preserved on Top-10/scanner; tooltip keeps the honest ASM/GSM caveat.

**Data note:** the prior session's FULL REBUILD (cached edge maps, gitignored) is on disk; a fresh clone must re-run
`backtest 5min/15min/30min/60min`. The finder/scanner caches are process-memory (rebuilt each `serve` start via warm).

## ◀ PREVIOUS SESSION (2026-06-28) — freshness panel + incremental onboarding + tradability + anchor re-baseline + full rebuild
Three NEXT-SESSION-PLAN P0s + a regression-anchor fix + the full universe rebuild (all now pushed; see commits
`fa80694`/`1845dda`/`7e3a6d7`). All display-only / signals-only; eligibility gate, Confidence, cost model untouched.
1. **P0-1 Edge-map freshness panel** — `EdgeMapMeta` sidecar (`save_edge_map_meta`) + `GET /api/edge_map_status` +
   `/` banner. Surfaces universe 1,634 vs 541 backtested (1,093 not yet onboarded), per-tf, new-since-build, stale files.
2. **P0-2 Incremental onboarding** — `strategy_engine::merge_edge_records` (byte-preserving text-splice for new symbols,
   validated, with safe re-serialize fallback) + `POST /api/onboard_symbol` + `add_stock` auto-chains download→onboard.
   Verified: onboarding 63MOONS preserved the full 5.78 MB map as an exact byte-prefix (5 eligible Short edges found).
   ⚠ **Effective in the live Top-10 only on `serve` restart** (in-memory universe is fixed at startup); immediate in the
   freshness panel + deep-dive. Hot-reload of the live universe was deliberately NOT done (dropped on review).
3. **P0-3 Tradability flags** — NEW `tradability.rs` (firewalled: imports only config/storage_kernel/kite_instruments).
   `GET /api/tradability` + warm cache + Top-10 badges (T2T/THIN/₹LOW + tooltip). Per symbol: series/T2T (from Kite
   tradingsymbol suffix), median ₹ turnover (nse_daily_all), last-price/micro-cap flags. **ASM/GSM = "not loaded"**
   (no local data — never fabricated). 1,782 symbols covered; 7 BE/T2T, 793 thin-or-worse. A caption, never a gate.
4. **Anchor re-baseline** — the `63MOONS·15m·n=51·+0.494R` figure was the *Python* project's anchor, never the Rust
   archive's. Codified the real Rust anchors as tests: `anchor_bajfinance_edge_map_stable` (edge-map tier) +
   `anchor_63moons_deep_dive_stable` (deep-dive tier, 30m/n=2603/conf=59). Both skip without the archive. See §6.
5. **FULL UNIVERSE REBUILD (user-requested)** — `backtest 5min/15min/30min/60min` over all **1,752** symbols (1day = 0,
   intraday strategies don't fire on daily). 30min: 541→**1,752** symbols, **1,632** eligible edges; 60min 1,861; 15min
   1,012; 5min 439. ~30min wall-clock total (30min tf alone = 23s; rayon saturates cores). Ran timeframes concurrently
   (separate output files = no race). **KEY FINDING:** the rebuild applies the CURRENT itemized cost; the cached maps were
   stale (pre-`7ec0a3f`), so every record's expectancy dropped ~0.01R (100% of 14,066 30min records lower, median
   −0.0102R, same n) — this forced the anchor re-baseline above. Eligible sets are CLEAN (max exp ≤1.1R; zero pathological
   records eligible). **Data-quality finding:** ~3–30 symbols/tf (CUPID, PRIVISCL, KAMOPAINTS…) produce non-finite/huge-
   negative metrics from likely-unadjusted corporate actions (handover P2 split-continuity) — correctly REJECTED by the
   gate, never in Top-10, but worth a split-continuity guard later. Backups of the pre-rebuild maps are in the session
   scratchpad. **Restart `serve` already done** — freshness panel `new_since_build`=0 across populated tfs.
**Not done (remaining P0):** P0-2b `add_stock` DETAILS onboarding (symbol_metadata upsert + sector + corp-actions +
indianapi fundamentals); tradability is wired to the Top-10 only — extend to scanner/desk/portfolio cards
(CapitalPick/RotationRow/HoldingAnalysis) as the plan specifies; add the split-continuity guard for the pathological names.

## ▶▶ NEXT-SESSION PLAN (specced 2026-06-28): backtest review + stock onboarding/enrichment
*Plan only — nothing here is built yet. Multi-agent reviewed (28 agents) against the actual code; every item below
passed an honesty-safety pressure-test. Execute in priority order. **Guardrails for ALL items:** never loosen the
eligibility gate, never let any new signal feed Confidence (= t-stat + behavioural penalties + DSR gate only), keep
everything display-only / no-orders, and keep the **Rust regression anchors green** (see §6 / UPGRADE_PLAN §0 —
re-baselined 2026-06-28: edge-map `BAJFINANCE·gap_and_go·Short·15min·n=130·exp=0.13012804335828682·PF=1.2383992474235814`
+ the 63MOONS deep-dive; both now RE-COMPUTE via the engine in `anchor_bajfinance_edge_map_stable` +
`anchor_63moons_deep_dive_stable`) — anything that moves numbers goes behind a flag + a SEPARATE cache file.*

### The factual picture today (corrected & verified)
- **Backtest universe behind the live Top-10 = the cached `cache/edge_map_30min.json`: 14,066 records over 541 distinct
  symbols, of which 170 carry an eligible edge (444 edges).** Top-10 Buy/Sell = best 10 of those 170 per side, by score.
- `discover_symbols` (reads `minute/`) now returns **1,558** symbols — so **~1,017 symbols on disk are NOT in the edge
  map** (incl. the owner's ~125 just-added stocks). The edge map only rebuilds when its file is *absent*; `add_stock`
  downloads candles but never re-backtests → new stocks are invisible to Top-10 until a manual `backtest 30min`.
- **Two backtest passes that disagree by construction:** (a) the cheap *edge-map* pass = 13 strategies × 2 dir × ONE
  fixed config (SL 1.5·ATR, RR 2.0), gate = `eligible()` (n≥30, PF≥1.2, exp>0) — **no OOS/DSR/WF at this tier**; (b) the
  *deep per-stock* pass (/intraday) = 4 strategies × 6 intervals × 5 R:R = up to 240 configs and is the ONLY place
  DSR/WF/purged-OOS/regime/slippage run. They also use **three different cost constants** (0.0013 / 0.0016 / 0.0012).
  So a name's Top-10 numbers ≠ what you see when you open it.
- **`passes_gate` (OOS exp>0, OOS n≥6) is documented as non-negotiable but is NOT enforced anywhere** — no such fn in
  `src/`. OOS is only a soft Confidence penalty in the deep pass; the Top-10 ranks on the un-deflated `eligible()` gate.
- **DSR effective-trials is mis-specified:** it counts every config's Sharpe (big_N≈240), not the documented ~12
  (interval × direction). Fixing it raises some Confidence scores → **needs an anchor re-baseline**.
- **Coverage hole:** only `30min` (live) + `15min` maps are populated; `60min`/`1day` are empty, `5min` has 0 eligible.
  The whole product rests on the 30min map.
- **Details on disk but unused by Rust:** `corporate_actions_all.parquet` (21k rows) — 0 Rust refs; `indianapi/` (1,932
  per-symbol fundamental JSONs: ROE/ROCE/PE/growth/analyst targets/shareholding) — unconsumed; sector missing for
  473/1558 names (30%); sector/thematic indices + INDIAVIX in `index_daily_all` — unused (RS is only vs NIFTY50).

### TASK 1 — Backtest method: more consistency + new validation strategies
- **[P0] Edge-map freshness + scope panel** (`cache/edge_map_{tf}.meta.json` at save; `GET /api/edge_map_status`;
  dashboard banner): surface `universe=1558 / backtested=541 / eligible=170 / NEW-since-build / files-changed / per-tf`.
  Pure honesty-layer; zero anchor risk. *Do this first — it makes every other gap visible.*
- **[P1] Robustness columns on the edge map (display-only):** extend `EdgeRecord` with `oos_expectancy`, `oos_n`,
  `wf_consistency`, and a per-symbol DSR over its own 26 strat×dir trials (reuse `validation.rs` + `stats.rs`). Annotate
  Top-10 with these but **do NOT change `eligible()`**. Closes the "Top-10 ranks on the weak gate" gap.
- **[P1] Rank on shrunk/deflated estimates + show CIs** (use existing `stats::shrunk_expectancy` + `expectancy_ci`):
  rank/tie-break Top-10 on the James-Stein-shrunk expectancy with a 90% CI, so small-n lucky configs stop topping the
  list (67 eligible edges have n<50). Display/ranking only — Confidence untouched.
- **[P1] CPCV + PBO panel (display-only):** new pure `src/cpcv.rs` (reuse embargo logic) → Probability of Backtest
  Overfitting % across combinatorial purged folds, on the deep-dive card. One-way import, never into `build_confidence`.
- **[P1] Intrabar resolution (P3) behind `--intrabar`:** wire `AmbiguityPolicy::IntrabarResolved` end-to-end (the
  `resolve_intrabar` stub + `SimConfig.finer` already exist) using `minute/` to learn which of stop/target printed
  first on ambiguous bars. **Opt-in, SEPARATE cache `edge_map_{tf}_intrabar.json`** — default pessimistic map (the
  anchor) stays byte-identical. Add the two UPGRADE_PLAN tests.
- **[P1] Next-bar-open entry policy** (`EntryFill::NextBarOpen` in `run_fill`): close the same-bar look-ahead (signal on
  a bar's close can't also fill at that close). **Opt-in + separate cache; never default** (moves R for almost every
  trade). Only ever makes numbers more honest.
- **[P1] Harden walk-forward for sparse/short history:** `walkforward_consistency` returns a neutral 1.0 when <2 folds
  populate → new stocks & sparse strategies get a free consistency pass. Return an "unknown" sentinel + require ≥5
  trades/fold. *Anchor-affecting (it's a Confidence input) — re-run both anchor tests + re-baseline if they move.*
- **[P2] Rolling edge-stability (expectancy decay)** as a display-only early-warning + optional tie-break only.
- **[Flagged, not a clean rec] Reconcile the two passes' cost constants (0.0013/0.0016/0.0012)** and the strategy/R:R
  contract so Top-10 ≈ drill-down. Careful: any change here moves numbers → anchor re-baseline. Treat as a deliberate,
  separately-reviewed refactor, not a quick fix.
- **[Flagged] Fix DSR effective-trials to ~12 (interval × direction)** per the invariant — raises some scores →
  requires an explicit anchor re-baseline + sign-off before flipping.

### TASK 2 — Include more stocks + more stock details (the newly-downloaded folder data)
- **[P0] Auto-onboard endpoint** `POST /api/onboard_symbol` (chain from `add_stock`): after `download_stock.py`, run
  `strategy_engine::backtest_symbol` for JUST that symbol on the live tf(s), then a new `merge_edge_records(new, tf)`
  that replaces only that symbol's rows and leaves all others byte-identical. Turns "download a stock" into "download →
  validate → can appear in signals" without a full-universe rebuild. LOW honesty risk (same gate/cost; per-symbol merge
  ⇒ zero drift for the 1,557 unchanged symbols, anchor safe).
- **[P0] `add_stock` also onboards DETAILS, not just candles:** extend `download_stock.py` (or a sibling
  `enrich_stock.py` the handler calls) to upsert the new symbol's row into `symbol_metadata.parquet`
  (sector/industry/mcap/name/isin from Yahoo `.info` + `recent_listings.csv`), append corp-actions, write its
  `daily_adj/` slice, optionally pull its indianapi fundamentals — then queue the re-backtest. Re-backtest MUST reuse the
  exact `backtest_universe` path (anchor byte-identical).
- **[P0] Tradability/surveillance/liquidity flag layer (display-only):** materialize `tradability.parquet` keyed by
  symbol (series EQ/BE/T2T, ASM/GSM, median ₹ turnover via `close*volume`, min-price floor); attach an optional
  `Tradability` to `BuyCandidate`/`CapitalPick`/`RotationRow`/`HoldingAnalysis` and render a non-blocking caption
  ("T2T — MIS may be rejected; verify"). **A caption, never a filter/gate/order.** Highest-consequence detail gap on a
  real-money board.
- **[P1] Materialize fundamentals** → flatten `indianapi/*.json` into one numeric `fundamentals.parquet` (pe, roe,
  roce, d/e, sales/eps growth, promoter %, analyst targets, as_of); add a firewalled `fundamentals.rs` loader and show
  as **display-only context** next to each edge/holding (~276 covered names; "no fundamentals" otherwise). Must NEVER
  enter Confidence, `eligible()`, or the planner's (price-only) score.
- **[P1] Detail-coverage panel + backfill the 473 missing sectors** (one DuckDB query → "sector known for X/N, mcap X/N,
  fundamentals X/N, edge map built T over M symbols"). Never fabricate a sector — widen coverage + report the residual.
- **[P2] Nightly full re-backtest safety net** (off-market ~02:00 IST, temp-file + atomic rename) under the incremental
  path, so symbols that cross the 100-bar/eligibility threshold get reconciled and all symbols share an as-of date.
- **[P2] Split-continuity check + corp-action context tag:** validate that intraday series with a post-2015 split show
  no uncorrected jump (the empirical pre/post≈1.0 property), flag/caption any that fail; surface dividends/splits as a
  small "recent corporate action" context tag. Display/validation only.

### Suggested execution order
1. **P0 freshness panel** (see the gap) → **P0 auto-onboard endpoint + merge** (get the +125 into Top-10) → **P0
   add_stock detail onboarding** → **P0 tradability flags**. This makes "add a stock" actually complete end-to-end.
2. Then **P1 robustness columns + shrunk ranking + fundamentals + coverage panel** (consistency + richer detail).
3. Then the **opt-in realism flags** (intrabar, next-bar-open) and **P2** items.
4. Only after explicit sign-off + anchor re-baseline: the two flagged number-moving items (cost reconciliation, DSR trials).

*Dropped on review (premise didn't hold in this codebase): "promote the deep-dive gauntlet into edge-map eligibility"
(would loosen the separation + risk the anchor), "best-TF-per-symbol consolidation", "hot-reload edge map in serve via
scheduler", "liquidity admission gate that prunes the universe" (prune = a gate; keep liquidity as a display flag, not a
filter). Full review JSON in this session's workflow output.*

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
6. **Regression anchor** (must stay byte-identical) — the Rust project's real anchor (UPGRADE_PLAN.md §0), now codified
   as tests so it can't silently drift:
   - **Edge-map tier (RE-BASELINED after the 2026-06-28 full rebuild):** edge `BAJFINANCE · gap_and_go · Short · 15min ·
     n=130 · exp=0.13012804335828682 · PF=1.2383992474235814` → `strategy_engine::tests::anchor_bajfinance_edge_map_stable`
     (now RE-COMPUTES via the engine, not the cache). The old `exp=0.1433565560483712`/SHA1s `34d4659c`/`a337c222` were a
     STALE pre-itemized-cost cache (see "FULL REBUILD" note below). New SHA1s: 15min `c7aaf1ae`, 30min `2a127eac`.
   - **Deep-dive tier (re-baselined 2026-06-28, 2,776 trading days):** `63MOONS · VWAP · SELL · 30 Minutes · +0.07R ·
     PF 1.18 · n=2603 · conf=59`; best overall = Prev-Day Breakout SELL 30m conf 59
     → `suggestion_engine::tests::anchor_63moons_deep_dive_stable`.
   - ⚠ The old `63MOONS · 15m · n=51 · +0.494R · conf=72` was the **Python** project's anchor, never the Rust archive's —
     retired here. A data refresh that moves `n_trades` is expected to require a conscious re-baseline of the deep-dive test.

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
