# Technical Reference

Everything technical in one place: vocabulary, formulas, models, data engineering, statistical methods, testable propositions, research angles, and failure modes. Numeric parameters below are **illustrative**. Always read live values from the protocol and its docs, since risk parameters change through governance.

---

## 1. Glossary

### Lending mechanics
| Term | Meaning |
|---|---|
| **Collateral** | Assets a borrower deposits to secure a loan |
| **Debt / borrow** | Amount owed, growing with accrued interest |
| **LTV (loan-to-value)** | Max borrowable value as a fraction of collateral value at borrowing time |
| **Liquidation threshold (LT)** | Debt/collateral ratio beyond which a position may be liquidated. LT > LTV, which creates the safety buffer |
| **Health factor (HF)** | Risk-adjusted collateral divided by debt. HF < 1 means liquidatable |
| **Liquidation bonus (penalty)** | Extra collateral value a liquidator receives, the incentive for acting |
| **Close factor** | Max fraction of the debt that can be repaid in one liquidation call |
| **Utilization (U)** | Borrowed / (cash + borrowed) for a reserve |
| **Reserve factor** | Share of interest kept by the protocol treasury |
| **Supply / borrow index** | Cumulative interest multipliers used to turn scaled balances into real balances |
| **Scaled balance** | Stored balance divided by the index at the time of the action |
| **eMode (efficiency mode)** | Aave mode with higher LTV for correlated assets (e.g. ETH and staked ETH) |
| **Isolation mode** | Aave feature restricting new collateral assets to a debt ceiling and limited borrowable assets |
| **Supply/borrow cap** | Hard limit on total deposits or borrows of an asset |
| **Bad debt / shortfall** | Debt not covered by collateral after liquidation |
| **Reserves / safety module** | Capital the protocol (or stakers) can use to cover shortfalls |
| **Flash loan** | Uncollateralized loan that must be repaid in the same transaction |
| **Isolated market** | Morpho-Blue style market with one collateral/loan pair and its own oracle and LLTV |
| **LLTV** | Liquidation LTV in Morpho Blue (parameter per market) |

### Risk and market terms
| Term | Meaning |
|---|---|
| **Liquidation cascade / spiral** | Liquidations sell collateral, price falls, more accounts become liquidatable |
| **Fire sale** | Forced selling at depressed prices |
| **Slippage / price impact** | Price movement caused by your own trade size |
| **Depeg** | A pegged asset trading away from its target (USDC/USD, stETH/ETH) |
| **Liquidator capacity** | The amount of debt liquidators can actually repay per unit time, bounded by capital, gas, and block space |
| **Time-to-liquidate** | Delay between HF < 1 and the liquidation transaction landing |
| **Gap risk** | Price jumping through the liquidation level without an intermediate fill |
| **MEV** | Value extracted by ordering/inclusion of transactions; liquidations are a major MEV source |
| **Whale** | Account with large debt or collateral relative to the market |
| **Correlated collateral** | Assets that move together, so diversification fails exactly when needed |
| **Wrong-way risk** | Collateral value falls when the borrower's ability to repay also falls |
| **Bank run** | Depositors withdraw simultaneously; at U ≈ 100% some cannot |

### Oracle terms
| Term | Meaning |
|---|---|
| **Heartbeat** | Max time between oracle updates even without price movement |
| **Deviation threshold** | Price move that triggers an update before the heartbeat |
| **Staleness** | Time since last update exceeds heartbeat expectations |
| **TWAP** | Time-weighted average price, resistant to single-block manipulation but laggy |
| **Sequencer uptime feed** | On L2s, a feed saying whether the sequencer is up; prices are unreliable right after downtime |
| **Circuit breaker** | Pause or clamp when the price moves implausibly fast |
| **Price band / sanity bounds** | Min/max acceptable price for a feed |
| **Fallback oracle** | Secondary source used if the primary fails |
| **Oracle extractable value (OEV)** | Value captured from the timing of oracle updates |

### Statistics and monitoring terms
| Term | Meaning |
|---|---|
| **VaR / ES (CVaR)** | Value at Risk (loss quantile) and Expected Shortfall (average loss beyond the quantile) |
| **EWMA** | Exponentially weighted moving average, used for volatility and control limits |
| **CUSUM** | Cumulative-sum test for detecting small persistent shifts |
| **Z-score** | Standardized deviation from a baseline |
| **GARCH** | Volatility model capturing clustering |
| **EVT / POT** | Extreme value theory / peaks-over-threshold, for tail modeling |
| **Hawkes process** | Self-exciting point process, a natural model for clustered liquidations |
| **Precision / recall** | Fraction of alerts that were real / fraction of real events alerted |
| **Lead time** | How long before the event the alert fired |
| **Alert fatigue** | Users ignoring alerts because too many are false |
| **HHI** | Herfindahl-Hirschman index, a concentration measure |

---

## 2. Core formulas

### 2.1 Health factor

For an account with collateral assets *i* and debt assets *j*:

```
HF = ( Σ_i  C_i · P_i · LT_i ) / ( Σ_j  D_j · P_j )
```

- `C_i` = collateral amount, `P_i` = oracle price, `LT_i` = liquidation threshold of asset i (only for assets enabled as collateral)
- `D_j` = debt amount including accrued interest
- HF < 1 → liquidatable

### 2.2 Real balances from scaled balances

```
real_supply  = scaled_supply × liquidity_index
real_debt    = scaled_debt   × variable_borrow_index
```

Indices grow continuously, so **debt grows with no event emitted**. Your engine must recompute indices at the evaluation timestamp (via rate and elapsed time), not just at the last event.

### 2.3 Distance to liquidation (single collateral, single debt, in USD)

```
HF = C·P·LT / D     →  liquidation price  P* = D / (C · LT)
percent drop to liquidation = 1 − P*/P  = 1 − 1/(HF)    [if only collateral price moves]
```

So HF = 1.25 means a 20% collateral drop reaches liquidation (1 − 1/1.25).

### 2.4 Liquidation depth curve

For each price grid point `p` of asset A (others held constant, or moved by a correlation model):

```
liquidatable_debt(p) = Σ_accounts  D_account · 1[ HF_account(p) < 1 ]
```

Plot `p` (or % drop) on x, `liquidatable_debt` on y. Variants: collateral-weighted, bonus-adjusted, close-factor-limited.

### 2.5 Liquidation mechanics

When liquidated with repayment `d` (≤ close_factor × D):

```
collateral_seized_value = d × (1 + bonus)
```

Liquidator profit (rough):

```
profit = d × bonus − gas_cost − slippage_cost − price_risk_while_holding − (flash loan fee, if used)
```

Liquidation is attempted **only if profit > 0**. If gas spikes or DEX liquidity thins, profitable liquidation vanishes for small positions, which is a main source of bad debt.

### 2.6 No-bad-debt condition (parameter sanity check)

At the moment of liquidation the price has moved a further fraction `s` below the HF=1 level (delay and gap). Collateral still covers repay-plus-bonus if:

```
C·P·(1 − s) ≥ D·(1 + bonus)       with  D = C·P·LT  at HF = 1
⟹  LT · (1 + bonus) ≤ (1 − s)
⟹  LT_max = (1 − s) / (1 + bonus)
```

Include recovery slippage `l` when the protocol or liquidator must sell collateral: `LT_max = (1 − s)(1 − l) / (1 + bonus)`. This yields the "implied required buffer" per asset: compare to the real LT and flag assets where `LT > LT_max` under plausible `s`.

Choose `s` from data: the worst price move over the realistic time-to-liquidate window (e.g. 99th-percentile and worst-ever move over N blocks).

### 2.6.1 Worked example (illustrative numbers)

LT = 0.80, bonus = 5%, shock before liquidation s = 10%:
`LT_max = 0.90 / 1.05 ≈ 0.857`. Since 0.80 < 0.857, an account at HF = 1 still covers debt plus bonus after a 10% gap. At s = 20%: `0.80 / 1.05 ≈ 0.762 < 0.80`, so the position would leave bad debt in a 20% gap.

### 2.7 Kinked interest rate model

```
if U ≤ U_opt:   R = R_base + (U / U_opt) · slope1
else:           R = R_base + slope1 + ((U − U_opt) / (1 − U_opt)) · slope2
```

Supply rate ≈ `R_borrow · U · (1 − reserve_factor)`.

Stress question: after the kink, does the steep slope pull U down fast enough (via repayments and new deposits) before withdrawals exhaust cash?

### 2.8 Withdrawal capacity and run risk

```
available_cash = total_supplied − total_borrowed     (per reserve)
run_pressure   = requested_withdrawals / available_cash
```

Estimate **time-to-full-utilization** from the recent trend of `dU/dt`, and from whales' share of deposits (a single large depositor leaving can spike U).

### 2.9 DEX price impact (constant-product approximation)

Selling `dx` of token X into a pool with reserves `(x, y)` and fee `f`:

```
dy = y · dx·(1−f) / ( x + dx·(1−f) )
effective_price = dy / dx
slippage = 1 − effective_price / spot_price
```

For concentrated-liquidity pools, integrate across ticks using the pool's liquidity distribution. For anything serious, quote via the actual pool contract on a fork (`quoteExactInput`) rather than trusting the approximation.

### 2.10 Cascade iteration (fixed-point)

```
P_0 = shocked price
repeat:
    L_k  = set of accounts with HF(P_k) < 1 (and profitable to liquidate, within capacity)
    S_k  = collateral sold by liquidating L_k
    P_{k+1} = P_0 · (1 − impact(S_k))      # impact from DEX model, plus cumulative sales
until  L_{k+1} ⊆ L_k  or  k > max_steps
```

Outputs: **cascade depth** (iterations), **extra price decline from cascade** (`P_0 − P_final`), **total liquidated**, **unliquidated underwater debt** (bad debt).

### 2.11 Bad debt estimate

For each underwater account at the final price:

```
bad_debt_account = max(0, D − recovered_collateral_value)
bad_debt_total   = Σ bad_debt_account
```

Report absolute, `% of TVL`, `% of reserves`, and `% of total borrows`. Always show **sensitivity** to slippage, liquidator count, and oracle lag.

### 2.12 Composite systemic risk score (transparent design)

```
score = Σ_k w_k · normalize(metric_k)      where metric_k ∈ {
    liquidatable_debt_ratio_at_20pct_drop,
    estimated_bad_debt_ratio,
    max_utilization,
    oracle_health_min,
    whale_concentration (top-10 debt share),
    liquidator_coverage_ratio^(-1)
}
```

Rules: weights are in config, each component shown in the dashboard, and the score is monotone in every input. Don't publish a score nobody can decompose.

---

## 3. Modeling angles (what to look at)

1. **Distance-to-liquidation mass.** Not "how many accounts are risky" but "how much debt is within X% of liquidation, and where does it cluster in price?" Clusters at specific prices are liquidation walls.
2. **Liquidator capacity vs. liquidation need.** A system with enough collateral but too little liquidator capital or block space fails anyway. Estimate capacity from historical liquidation throughput per block and the top liquidator wallets' balances.
3. **Gas as a risk factor.** During crashes gas rises; liquidation profitability for small positions disappears. Include a gas multiplier in every stress scenario.
4. **Correlation breakdown.** Assume correlations rise toward 1 in crashes. Never use calm-period correlations for stress. Use crisis windows.
5. **Oracle lag as a hidden shock.** Oracle price lags market price by up to the deviation threshold. A 0.5% deviation threshold plus heartbeat gaps can mean the protocol sees stale prices exactly when speed matters.
6. **Wrong-way and loop risk.** Borrow ETH against stETH, redeposit, repeat: positions are safe until stETH/ETH depegs, then HF collapses under *no change* in USD prices. Model pair-ratio shocks, not only USD shocks.
7. **Concentration.** One account holding a large fraction of a reserve's borrows or of a collateral's supply is a single point of failure. HHI and top-N shares.
8. **Utilization dynamics.** High U is not a risk by itself (it earns yield), but it combined with whale deposit exits and slow repayment is. Model deposit exit as a hazard.
9. **Governance and parameter lag.** Risk parameters update slowly vs. market conditions. Track parameter-change events as state changes.
10. **L2 specifics.** Sequencer downtime, delayed price updates after restart, and different gas dynamics change the risk profile. Monitor the sequencer uptime feed.
11. **Cross-protocol exposure.** The same entity may carry risk on several protocols; its liquidation on one can trigger sales that move collateral prices on all.
12. **Reflexivity of the monitor itself.** Public alerts about whale vulnerability can be traded against (front-running, deliberate price attacks). Consider delaying or limiting public detail about specific accounts.

---

## 4. Statistical and ML methods (use simple first)

| Need | Start with | Later |
|---|---|---|
| Volatility estimate for shock sizing | EWMA, rolling std | GARCH, realized volatility from high-frequency data |
| Tail risk of collateral | Historical VaR/ES | EVT (peaks-over-threshold, GPD fit) |
| Anomaly in a metric | Z-score vs. rolling baseline, EWMA control chart | CUSUM, isolation forest, seasonal decomposition |
| Clustering of liquidations in time | Simple counts per window | Hawkes process |
| Correlation under stress | Correlation on crisis windows | Copulas (t-copula for tail dependence) |
| Depeg detection | Price deviation from peg beyond band | Regime-switching / hidden Markov model |
| Cascade realism | Deterministic fixed-point loop | Agent-based simulation with heterogeneous liquidators |
| Early-warning classification | Rule-based thresholds | Logistic regression or gradient boosting on engineered features, with calibration |

**Warning:** with only a handful of historical crashes, complex ML will overfit. Prefer interpretable rules and report uncertainty honestly. Use **time-based train/test splits** (never random), and tune on one event and test on another.

### Alert evaluation framework

- **Event definition:** pick explicit ground truth (e.g. "bad debt > $X," "liquidations > Y% of borrows in 24h," "utilization > 99% for ≥ 1h").
- **Lead time** = `t_event − t_first_alert` (positive = early).
- **Precision** = true alerts / all alerts; **Recall** = events alerted / all events.
- **False positives per week** on calm periods.
- **Cost-sensitive view:** a missed crash costs far more than a false alarm, but too many false alarms get ignored. State the trade-off and pick thresholds deliberately.
- **Severity calibration:** a "critical" alert should be rare (target e.g. < 1/month on calm markets).

---

## 5. Data engineering notes

### 5.1 Data sources
- **Event logs** from protocol contracts (primary source of truth)
- **Contract reads** (`eth_call`, batched with Multicall3) for reserve data, oracle prices, and verification
- **Subgraphs / Dune / Flipside** for historical backfills and cross-checks
- **DEX pools** for liquidity depth (Uniswap v2/v3 reserves and tick data, Curve pool state)
- **Price feeds**: Chainlink on-chain; optionally CEX REST/WebSocket for reference price
- **Gas data**: base fee and priority fee history
- **Archive node or provider** for historical state at arbitrary blocks (needed for backtests)

### 5.2 State reconstruction strategies
- **Event-sourced:** replay all events from deployment. Exact but must handle every event type and upgrade.
- **Snapshot plus incremental:** periodically read full state via contract calls and apply new events. Good for faster cold start.
- **Hybrid verification:** run a background job comparing sampled accounts with on-chain `getUserAccountData` and alert on mismatch beyond tolerance.

### 5.3 Correctness hazards
| Hazard | Mitigation |
|---|---|
| **Chain reorgs** | Store block hash per row; on mismatch roll back to common ancestor; treat the last N blocks as unconfirmed |
| **Duplicate or out-of-order events** | Idempotent writes keyed by `(block, tx_index, log_index)` |
| **Decimals and rounding** | Use integer math (U256) or decimals libraries in the indexer; convert for display last. Match the protocol's rounding (ray/wad math) in golden tests |
| **Proxy upgrades changing ABIs** | Version adapters by block range |
| **Missing implicit interest accrual** | Recompute indices at query time |
| **RPC inconsistency** | Multi-provider quorum for critical reads; compare block numbers |
| **Backpressure during busy blocks** | Bounded queues, batch writes, drop-nothing policy with lag metrics |
| **Clock skew** | Use block timestamps for logic, wall clock only for ops metrics |

### 5.4 Suggested schema (sketch)

```sql
-- reserves: one row per (protocol, asset, block)
reserve_state(protocol, chain_id, asset, block_number, block_hash, ts,
              liquidity_index, borrow_index, total_supply, total_borrow,
              utilization, borrow_rate, supply_rate, ltv, liq_threshold,
              liq_bonus, supply_cap, borrow_cap, is_frozen, is_paused)

-- positions: current state per account/asset (+ history via hypertable)
position(protocol, chain_id, account, asset, scaled_supply, scaled_debt,
         collateral_enabled, last_block)

-- oracle
oracle_update(feed, block_number, ts, answer, round_id, updated_at)

-- liquidation events (for liquidator behavior and backtests)
liquidation(protocol, block_number, tx_hash, account, collateral_asset,
            debt_asset, debt_repaid, collateral_seized, liquidator, gas_price)

-- computed
risk_snapshot(ts, protocol, metric, params_json, value)
alert(id, ts, rule_id, severity, metric_value, threshold, status, payload_json)
```

Use **TimescaleDB hypertables** partitioned by time for `reserve_state`, `oracle_update`, `risk_snapshot`, and **continuous aggregates** for the dashboard's charts.

### 5.5 Performance notes
- Batch reads with **Multicall3**; cache static parameters
- Don't recompute all accounts on every block: maintain a **priority index** by distance-to-liquidation and recompute exactly only those near the threshold; recompute everything on a slower schedule
- Vectorize depth curves with numpy/polars; push the hot loop into Rust (PyO3) only after profiling

---

## 6. Propositions (hypotheses to test)

Treat these as research questions the project can answer with data. Each should end as "supported / not supported / inconclusive" in your write-up.

| # | Proposition | How to test |
|---|---|---|
| P1 | Liquidation depth near the current price (within 10-20%) predicts subsequent bad-debt severity better than average HF | Compare predictive power on historical crashes |
| P2 | Oracle deviation vs. DEX price rises before or during a depeg early enough to give actionable lead time | Replay USDC March 2023 and stETH June 2022 |
| P3 | Utilization above the kink combined with high deposit concentration precedes withdrawal stress | Event study on reserves with U > 95% |
| P4 | Including gas and liquidator capacity in stress tests changes bad-debt estimates materially (>2×) vs. naive "instant liquidation" | Ablation study on the same snapshot |
| P5 | Correlated-collateral loops (LST/ETH) dominate tail risk in "ETH-denominated" positions | Pair-ratio shocks vs. USD shocks |
| P6 | Rule-based alerts with 2-3 features match ML classifiers on out-of-sample crashes | Time-split evaluation |
| P7 | Small positions go unliquidated in high-gas regimes, accounting for most bad-debt *count* but little bad-debt *value* | Analyze historical liquidations vs. gas |
| P8 | A single whale (top 1 borrower) can account for >X% of a reserve's stress-test bad debt | Concentration decomposition |
| P9 | Time-to-liquidate (HF<1 to tx) lengthens sharply during crashes | Measure from on-chain data |
| P10 | Stress-test outputs are more sensitive to the liquidator-behavior assumption than to the price-shock size | Sensitivity analysis (tornado chart) |

---

## 7. Failure modes of Tahadhari itself

| Failure | Consequence | Mitigation |
|---|---|---|
| Indexer lag | Alerts arrive late, which defeats the product | Lag metric as a first-class alert; "monitor the monitor" |
| State drift vs. chain | Wrong health factors | Continuous sampled verification; auto-resync |
| RPC outage | Blind spots | Multi-provider failover; heartbeat alert if no new blocks processed |
| Wrong parameters (stale LT) | Systematically wrong results | Read parameters from chain every N blocks; version them |
| Alert storm | Users mute it | Dedup, cooldown, severity escalation, grouping |
| Model overconfidence | False reassurance | Always display assumptions and ranges, not point estimates |
| Single point of failure | Silent death | Dead-man switch: a separate tiny service alerts if no heartbeat from the main system |
| Secret leakage | Compromised keys | `.env` only, secret scanning in CI, minimal-permission keys; keeper has no keys on mainnet |
| Manipulation of monitored data | False alarms | Use TWAPs and multi-source checks for inputs |

---

## 8. Attack and failure scenarios for the Foundry lab

Build each against `MockLendingPool.sol`, show the exploit or failure, then the mitigation:

1. **Stale oracle** → borrow against inflated collateral value or block valid liquidations. *Mitigation:* staleness check against heartbeat, revert or pause.
2. **Spot-price oracle manipulation with a flash loan** → pump a thin pool, borrow against it, and leave bad debt. *Mitigation:* TWAP, deviation check against a second source.
3. **Depeg** of collateral or debt asset → HF drops without USD price move. *Mitigation:* pair-aware pricing, tighter eMode parameters, circuit breaker.
4. **Donation attack / share inflation** on first-depositor reserves. *Mitigation:* virtual shares, minimum liquidity, burn initial shares.
5. **Rounding exploitation** in repeated small borrows or repays. *Mitigation:* round in the protocol's favor, add invariant tests.
6. **Liquidation griefing**: tiny positions that cost more to liquidate than they are worth, accumulating dust bad debt. *Mitigation:* minimum borrow size, dust-liquidation rules.
7. **Reentrancy** via callback tokens during liquidation. *Mitigation:* checks-effects-interactions, reentrancy guard.
8. **Sequencer downtime** (L2 variant): prices stale, liquidations blocked, then a stampede after restart. *Mitigation:* grace period after sequencer restart.

Invariants to fuzz: `Σ user_balances ≤ total_underlying`, `total_debt ≥ 0`, `no account with HF<1 after a successful borrow/withdraw`, `interest index monotonic non-decreasing`, `solvency: assets ≥ liabilities − bad_debt_recorded`.

---

## 9. Alert catalogue (starter set)

| ID | Signal | Default severity |
|---|---|---|
| `oracle.stale` | Feed not updated > 1.5× heartbeat | high |
| `oracle.deviation` | Feed vs. DEX/CEX > 1% for > N minutes | high |
| `oracle.sequencer_down` | L2 sequencer feed reports down | critical |
| `depeg.warn` | Stablecoin or LST off peg beyond band | warning → critical on size |
| `util.high` | Reserve U > kink for > 1h | warning |
| `util.runway` | Projected time-to-100% < 6h | high |
| `depth.20pct` | Liquidatable debt at −20% > threshold | high |
| `whale.hf` | Top-N borrower HF < 1.1 (non-public detail) | warning |
| `cascade.depth` | Stress cascade depth > threshold | high |
| `baddebt.est` | Estimated bad debt in default scenario > threshold | critical |
| `liq.spike` | Liquidation volume > k-sigma above baseline | warning |
| `liq.unprofitable` | Gas high enough that median liquidation is unprofitable | high |
| `param.change` | Governance changed LTV/LT/caps for an asset | info |
| `system.lag` | Indexer behind head by > N blocks | critical (ops) |
| `system.silent` | No events processed in > N minutes | critical (ops) |

---

## 10. Kenyan angle (optional extension)

- **Borrower-side currency mismatch:** model borrowers whose income is in KES but whose debt is in USD stablecoins. KES/USD depreciation lowers repayment capacity even when collateral is stable.
- **On/off-ramp fragility:** M-Pesa/P2P ramps can freeze or widen spreads during stress, which slows deleveraging and repayment. Add ramp-availability as an exogenous stress variable.
- **Thin local liquidity:** KES-denominated pools are shallow; slippage assumptions must be far harsher than ETH/USDC.
- **Regulation:** if the work ever moves from research to serving users or handling funds, review Kenya's Virtual Asset Service Providers Act and seek legal advice.

---

## 11. Extensions after v1.0

- Agent-based simulator (borrowers, liquidators, arbitrageurs, depositors) to replace deterministic cascade assumptions
- Parameter optimizer: search LTV, LT, bonus, and close factor minimizing bad debt subject to capital efficiency
- Address clustering for cross-protocol whale exposure
- MEV-aware liquidation timing model (bundles, priority fees, builder behavior)
- Public risk reports published monthly (builds credibility and an audience)
- Support for lending on Solana (different account model) as a comparative study

---

## 12. Reading list (technical)

- Aave v3 docs and risk framework; Aave governance risk parameter proposals (good real examples of how LT/bonus are chosen)
- Compound III (Comet) docs; Morpho Blue whitepaper
- Chainlink data feed docs: heartbeat, deviation, L2 sequencer uptime feeds
- MakerDAO Black Thursday post-mortem; Gauntlet and Chaos Labs public risk write-ups
- Academic work on DeFi liquidations, "fire sales," and liquidation-driven price spirals
- Uniswap v2/v3 whitepapers (price impact math)
- Foundry Book (fuzzing and invariant testing)

---

## 13. Language boundaries and messaging (polyglot design)

### 13.1 Who decides what
| Concern | Owner | Reason |
|---|---|---|
| Computing metrics, z-scores, projected runway, scores | Python engine | Numerical libraries, easy experimentation |
| Thresholds, durations, cooldowns, dedup, escalation, routing | Go alert service | Rules change often and delivery is concurrent I/O |
| Exact on-chain state | Rust indexer | Integer-exact math and reorg-safe processing |
| Serving dashboards | Go API | Many concurrent connections |
| Transaction construction | Go keeper | `go-ethereum` and `abigen` |

### 13.2 Messaging rules
- **Redis Streams with consumer groups**, so a restarted consumer resumes from its last acknowledged message. Pub/sub drops messages on disconnect, so avoid it for anything important.
- Each message carries `schema_version`, `produced_at` (block timestamp for chain-derived data), `protocol`, `chain_id`, and a `trace_id` for following one event across services.
- **Idempotent consumers:** the same message may be delivered twice, so deduplicate by `(stream, message_id)` or a business key.
- **Backpressure:** bounded stream length (`MAXLEN ~`) plus a lag metric; if a consumer falls behind, alert on it instead of silently dropping.
- **Time:** use block timestamps for logic and wall clock only for operations metrics.

### 13.3 Schema evolution
- Add fields freely, never repurpose or remove within a version, and bump to `.v2` for breaking changes, running both during migration.
- Generate types for Rust (serde), Go (structs), and Python (pydantic) from one source in `schemas/`; CI fails if generated code is stale.

### 13.4 Example `metric.v1`
```json
{
  "schema_version": 1,
  "trace_id": "01J...",
  "protocol": "aave_v3",
  "chain_id": 1,
  "block_number": 21000000,
  "ts": "2026-11-24T10:00:12Z",
  "metric": "stress.liquidatable_debt_ratio",
  "params": { "asset": "WETH", "shock": -0.20 },
  "value": 0.043,
  "unit": "ratio",
  "assumptions_hash": "ab12..."
}
```

### 13.5 Testing across boundaries
- **Contract tests:** each producer validates its output against the JSON Schema; each consumer validates input and rejects unknown major versions.
- **End-to-end test on a fork:** change the mock oracle price, then assert a Telegram-bound alert (to a fake notifier endpoint) arrives within a latency budget. Run it in CI nightly.
- **Replay determinism:** the same recorded streams must yield the same metrics and alerts (Python engine) and the same alert decisions (Go evaluator).

### 13.6 Solidity learning checklist (for the first-timer)
- [ ] Value types, `storage`/`memory`/`calldata`, mappings, structs, events, custom errors
- [ ] Visibility, modifiers, inheritance, interfaces, `immutable`/`constant`
- [ ] ERC-20 mechanics; `SafeERC20`; why `approve` and `transferFrom` exist
- [ ] Fixed-point math: wad (1e18) and ray (1e27), rounding direction
- [ ] Reentrancy and checks-effects-interactions
- [ ] Foundry: `forge test`, `vm.warp/prank/deal/mockCall/expectRevert`, fuzzing, invariants, fork tests
- [ ] Reading: Aave v3 `Pool`, `ReserveLogic`, `GenericLogic`; Compound `Comet`; Morpho Blue
- [ ] Post-mortems: Euler, Cream, Beanstalk, Mango (as a non-EVM contrast)