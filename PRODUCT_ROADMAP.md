# RAM_ISTP — Product Roadmap & Competitive Positioning

*Honesty-first NSE analytics. Signals-only: no order button, no fund to sell, no advice.*
*From a multi-agent survey (Groww / Zerodha / INDmoney) + grounded feature design, 2026-06-28.*

## 1. Competitive positioning

| Platform | What they own | The honest gap WE own |
|---|---|---|
| **Groww** (~1.4cr clients) | Lowest-friction single app; direct-MF + flat fees; fast screeners + curated buckets; auto "Pros/Cons" | Screeners flag RSI/MACD/breakouts as **facts, not bets** — no t-stat, no sample size, no DSR/multiple-testing penalty, no net-of-cost edge, no regime tag. (Also an AMC → structural conflict.) |
| **Zerodha** (Kite+Console+Coin) | Exchange-grade ledger: FIFO tax P&L, corporate-action XIRR, GTT/Cover/basket, open API | Console's performance curve **excludes volatility, Sharpe, max-drawdown**; no risk-of-ruin/sizing, no concentration/correlation, no edge-validity; after-cost only *after the fact* |
| **INDmoney** (10M+) | Best **aggregation**: net-worth roll-up across brokers/AMCs, fund-overlap, MF risk ratios | Stock "insights" = analyst-consensus + star-rating ("93.75% BUY" reads as certainty); no edge-fragility, no after-cost on the idea, no portfolio **risk** picture; stale CAS shown as live |

**Our one-sentence moat:** *RAM_ISTP is the only NSE tool with no fund to sell and no order button that shows you both whether an edge is real (Confidence: t-stat + behavioral penalties + DSR gate) and whether your own portfolio is fragile (concentration, correlation, risk-of-ruin) — every number net of cost, timestamped, caveated, and strictly your decision.*

## 2. Shipped this session

- **Holdings risk picture** (`holdings_analytics.rs`, `POST /api/holdings`, Desk tab): the half of portfolio analytics the big 3 omit — concentration (HHI/effective-names), sector & broker heat, same-sector clusters, per-name WHY flags, edge cross-reference, half-Kelly **advisory** band. Multi-input (CSV/text/sample). Display-only, no advice.
- **High-conviction shortlist** (`stats::is_high_conviction_shortlist`, badge + caveat): the honest ">60%" — Confidence ≥70 AND Wilson win-floor ≥60% AND DSR gate. A shortlist with a fixed "not a sure shot" caveat, never feeds Confidence. (Today flags 0 names — the truthful answer.)
- Plus this session's earlier work: warm-cache latency (30–60s→~1–4ms), parallelized deep-dive, robust backtester (ambiguity flag, slippage stress band), scanner/deep-dive DSR reconciliation, NSE instrument-token mapping, market-hours gating, news budget guard, live Kite feed (`ram_istp live`, TLS fixed).

## 3. Roadmap — next (specced, not yet built)

**Track 3 — edge analytics (cheap, high value):**
- C1 **why-this / why-not** inline: surface the Conviction deltas + the dominant Confidence penalty (`build_confidence` already returns `penalties`; `ensure_confidence` currently drops them).
- C2 **regime-conditional display**: `analyze_symbol` already splits trades by NIFTY up/down regime then discards them — capture into a `RegimeSplit` and render "Up +0.42R (n=120) · Down +0.05R (n=64)". Display-only.
- C3 **correlation-aware exposure** on the live Top-10: an info Alert ("3 of your top picks are PSU banks — ~1 bet, not 3"). Never prunes/sizes.

**Area D — bigger gaps (need a design pass):**
- D1 **Live LTP + bid/ask spread + tradability flags (T2T/ASM/GSM/circuit)** on cards before staging — the Tier-1 "can't trade live without it" gap.
- D2 after-cost/slippage band surfaced *at the signal* on scanner rows (deep-dive already has it).
- D3 **aggregate portfolio risk-of-ruin / drawdown-survival** (reuse the seeded MC engine on the real book).
- D4 watchlists beyond the fixed universe; event-risk calendar (earnings/ex-date); news sentiment ON.
- D5 read-only CAS/Account-Aggregator import to match INDmoney's aggregation reach.

## 4. Scenarios the platform now enables

1. *"Show me my portfolio's risk, not just its growth."* Paste a Zerodha+Groww CSV (or Load sample) → Desk shows value vs cost, HHI label + effective-names, top-3 weight, per-name deep-loss/concentration/sector/no-edge flags with the WHY, a half-Kelly **advisory** band, sector/broker heat, correlation clusters — every value `built <IST>`, EOD marks badged. *Groww/Console/INDmoney show none of this risk view.*
2. *"Three of these are the same bet."* Sector heat shows Banking 50% + a cluster grouping SBIN+ICICIBANK+HDFCBANK — honest about *why* (same sector), not overstating a correlation it didn't measure.
3. *"Give me today's >60% shortlist."* Scanner ★-filters to setups clearing Confidence ≥70 + win-floor ≥60% + DSR gate. Today: **0 qualify** — the platform tells you the truth (no sure shots) instead of manufacturing one.

## 5. Honesty guardrails (the lines we will not cross)

1. **Confidence = t-stat + behavioral penalties + DSR gate ONLY.** Holdings metrics, the shortlist flag, regime splits — all computed *after* scoring; structurally cannot change it (firewall tests + byte-identical anchor).
2. **No advice, ever.** Flags carry a WHY, never a directive. Kelly is half-Kelly, clamped, a band, labelled *advisory*; [0,0] for edgeless names.
3. **No naked BUY/SELL, no "sure shot."** High Confidence + high probability = a caveated shortlist.
4. **Signals-only.** Nothing places/modifies/cancels an order anywhere. `/api/holdings` is read-only display JSON.
5. **Staleness explicit** (`mark_is_live` + `built_ist`); **no secrets logged**; **best-effort parsers admit it** (warnings, never fabricate).
6. **Not a licensed advisor.** This tool surfaces evidence; the user makes every decision.
