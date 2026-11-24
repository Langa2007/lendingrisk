# LENDINGRISK: Real-Time Early Warning System for DeFi Lending Risk

> 

Tahadhari watches on-chain lending protocols (Aave, Compound, Morpho and similar) in real time and warns **before** risk turns into bad debt. It indexes borrower positions and oracle prices, computes health and systemic-risk metrics continuously, simulates "what if price falls X% right now?", and fires alerts when the system gets fragile. An optional module acts as a liquidation keeper.

---

## 1. The problem

Most lending protocol failures are **risk failures**, not coding bugs:

- A price falls faster than liquidators can act, so collateral becomes worth less than debt (**bad debt**).
- An oracle is stale, manipulated, or reports a depegged asset, which triggers wrong liquidations or allows bad borrows.
- Utilization hits ~100%, depositors cannot withdraw, and a bank run begins (**liquidity risk**).
- Many large positions are correlated (e.g. leveraged staked-ETH loops), so one shock liquidates them all at once (**cascade**).
- Liquidations become unprofitable because gas spikes or DEX liquidity thins, so nobody liquidates (**liquidator failure**).

Existing dashboards show **current** state. Tahadhari answers **forward-looking** questions:

1. How much debt becomes liquidatable if collateral falls 10%, 20%, 30%?
2. Is there enough liquidator capacity and DEX depth to absorb it?
3. Which oracle feeds are drifting, stale, or deviating from market?
4. Which whales are close to liquidation, and would their liquidation move the market?
5. How close is the system to a bank-run condition?

---

## 2. What it does

| Capability | Description |
|---|---|
| **Position indexer** | Streams borrow, supply, repay, liquidate events and reconstructs every account's collateral and debt |
| **Oracle monitor** | Tracks Chainlink and other feeds: staleness, deviation vs DEX/CEX price, heartbeat misses, depeg signals |
| **Health engine** | Computes health factor per account and aggregate liquidation depth curves |
| **Stress engine** | Instant price-shock scenarios (single asset, correlated basket, depeg, gas spike) with estimated bad debt |
| **Liquidity monitor** | Utilization, withdrawal capacity, rate-curve position, time-to-100% utilization |
| **Alert engine** | Rule-based and statistical alerts with severity, dedup, and routing (Telegram, Slack, webhook, email) |
| **Dashboard** | Live charts: liquidation depth, whale watchlist, oracle health, utilization, active alerts |
| **Keeper (optional, Go)** | Simulates and executes profitable liquidations on testnets/forks, and measures how fast they happen |
| **Backtester** | Replays historical crashes (Black Thursday 2020, Terra/UST 2022, USDC depeg 2023) to check that alerts would have fired in time |

---

## 3. Architecture overview

```
                ┌────────────────────────────────────────────┐
                │  Ethereum / L2 nodes (RPC + WebSocket)     │
                │  Protocol subgraphs, price APIs, DEX pools │
                └──────────────────┬─────────────────────────┘
                                   │
                   ┌───────────────▼───────────────┐
                   │   Indexer  (RUST)             │
                   │   events → normalized state   │
                   └───────┬───────────────┬───────┘
                           │               │
                ┌──────────▼───┐     ┌─────▼──────────────────┐
                │  Postgres /  │     │  Redis Streams         │
                │  Timescale   │     │  (hot state, messaging)│
                └──────────┬───┘     └─────┬──────────────────┘
                           │               │
              ┌────────────▼───────────────▼─────────────┐
              │   Risk Engine  (PYTHON)                  │
              │   health · oracle · liquidity · stress   │
              │   stats · scoring · KES module           │
              └────────────┬─────────────────────────────┘
                           │  metrics
          ┌────────────────┼──────────────────┐
          │                │                  │
   ┌──────▼──────┐  ┌──────▼───────┐   ┌──────▼──────┐
   │ Alerts (GO) │  │  API (GO)    │   │ Keeper (GO) │
   │ rules,dedup,│  │  HTTP + WS   │   │ optional,   │
   │ notifiers   │  └──────┬───────┘   │ fork-only   │
   └─────────────┘         │           └─────────────┘
                    ┌──────▼──────┐
                    │  Dashboard  │   Contracts lab (SOLIDITY / Foundry)
                    │ (TypeScript)│   tests attack scenarios and math
                    └─────────────┘
```

Full detail is in `docs/TECHNICAL.md`; the build order is in `docs/PHASES.md`; the file layout is in `docs/REPO_STRUCTURE.md`.

---

## 4. Tech stack

| Layer | Choice | Why |
|---|---|---|
| Indexer / ingestion | **Rust** (`alloy`, `tokio`, `sqlx`) | Integer-exact math, throughput, long-running reliability |
| Risk engine | **Python** (`numpy`, `pandas`, `scipy`, `polars`) | Modeling speed; hot loops can move to Rust via PyO3 later |
| Alert service | **Go** | Concurrent notifiers, consumer groups, simple deployment |
| API / WebSocket gateway | **Go** | Many concurrent dashboard connections |
| Keeper (optional) | **Go** (`go-ethereum`, `abigen`) | Mature Ethereum tooling for transaction bots |
| Contract lab | **Solidity + Foundry** | Fuzz, invariant, and fork testing |
| Dashboard | **React + TypeScript** | Interactive charts |
| Storage / messaging | **PostgreSQL + TimescaleDB**, **Redis Streams** | Time-series history, durable messaging |
| Infra | **Docker Compose**, GitHub Actions, Prometheus + Grafana | Reproducible and observable |

**Why four languages in one project:** each owns whole services and they communicate only through Postgres, Redis Streams, and versioned schemas in `schemas/`, so no language needs to import another's code. Details are in `docs/REPO_STRUCTURE.md`, and the learning order is in `docs/PHASES.md`.

---

## 5. Quick start (target experience)

```bash
git clone https://github.com/<you>/tahadhari.git
cd tahadhari
cp .env.example .env          # add RPC URL(s) and optional API keys
docker compose up -d          # postgres, redis, indexer, risk engine, api, dashboard
make migrate                  # create schema
make seed                     # backfill recent history for the chosen protocol
open http://localhost:3000    # dashboard
```

Run on a **mainnet fork** for safe experimentation:

```bash
make fork                     # anvil --fork-url $RPC_URL
make scenario NAME=eth-30-drop
```

---

## 6. Configuration

All configuration lives in `config/`:

- `protocols.yaml`: which protocols, chains, contract addresses, and parameters to watch
- `alerts.yaml`: thresholds, severities, cooldowns, routing
- `scenarios/*.yaml`: stress scenarios (shock size, correlation, gas multiplier)
- `.env`: secrets (RPC keys, bot tokens). Never commit this file.

Example alert rule:

```yaml
- id: liquidation-depth-eth-20
  description: Debt liquidatable under a 20% ETH drop exceeds 5% of total borrows
  metric: stress.liquidatable_debt_ratio
  params: { asset: WETH, shock: -0.20 }
  condition: "> 0.05"
  severity: high
  cooldown: 30m
  route: [telegram, slack]
```

---

## 7. Metrics Tahadhari produces

- **Health factor distribution** across all accounts (histogram, by size-weighted debt)
- **Liquidation depth curve**: debt liquidatable at each price level, per collateral asset
- **Estimated bad debt** under each scenario, as a fraction of TVL and of reserves
- **Liquidation coverage ratio**: liquidatable collateral vs. available DEX depth and liquidator capacity
- **Utilization and time-to-full-utilization** under current borrow/repay flow
- **Oracle health score**: staleness, deviation, heartbeat adherence, depeg probability
- **Concentration indices**: top-N borrower share, collateral correlation exposure (HHI)
- **Composite systemic risk score** (0-100) with a transparent breakdown

---

## 8. Roadmap

See `docs/PHASES.md`. Summary:

| Phase | Outcome | Language focus |
|---|---|---|
| 0 | Foundations and Solidity ramp | Solidity |
| 1 | Indexer and position state | Rust |
| 2 | Health engine and liquidation depth | Python |
| 3 | Oracle monitor | Python / Rust |
| 4 | Liquidity, run risk, concentration | Python |
| 5 | Stress, cascade, bad debt | Python |
| 6 | **Alert service (MVP)** | Go |
| 7 | API, dashboard, observability | Go / TypeScript |
| 8 | Contract attack lab | Solidity |
| 9 | Backtesting | Python |
| 10 | Keeper (optional) | Go |
| 11 | Multi-protocol, L2, loops, cross-protocol | All |
| 12 | Simulation and parameter optimizer (optional) | Python |
| 13 | KES module, hardening, publication | All |

---

## 9. Testing and validation

- **Unit tests** for every formula (health factor, interest accrual, liquidation math) against known protocol values
- **Differential tests**: your computed health factor vs. the protocol's own `getUserAccountData`
- **Replay tests**: feed historical blocks and verify the reconstructed state matches on-chain state
- **Backtests**: measure *lead time* (how long before the crash the alert fired) and *false positive rate*
- **Foundry invariant/fuzz/fork tests** for the mock pool and for comparing your math to real protocol outputs
- **Go tests with `-race`** for alert and API services; **schema CI** that fails if generated types drift
- **Chaos tests**: drop RPC connections, reorder blocks, duplicate events, and confirm state stays correct

---

## 10. Safety, ethics, and legal notes

- Tahadhari is **read-only** by default. The keeper runs only on testnets or forks unless you explicitly extend it.
- Alerts are **informational, not financial advice**. State this in the dashboard and alert text.
- Never commit private keys or RPC secrets. Use `.env`, and a secrets manager if you deploy.
- If you ever hold or move real funds, or offer this as a paid service to users in Kenya, check obligations under Kenya's Virtual Asset Service Providers Act and consult a lawyer. Monitoring public data for your own research is a different matter.
- Respect API rate limits and terms of service of data providers.

---

## 11. Known limitations (be honest about these in your write-up)

- Reconstructed state can lag or drift on chain reorgs; reconciliation jobs are required.
- Stress estimates depend on assumed DEX depth and liquidator behavior, which are models, not facts.
- Off-chain CEX liquidity is invisible to on-chain data.
- Cross-protocol exposure of one wallet (same whale borrowing on three protocols) needs address clustering, which is imperfect.
- Alerts are probabilistic; some will be false alarms.

---

## 12. Contributing and license

Open an issue before large changes. Add tests with every PR. Suggested license: **MIT** or **Apache-2.0**.

---

## 13. References to study

- Aave v3 technical paper and risk documentation
- Compound v2/v3 documentation
- Morpho Blue whitepaper
- Gauntlet and Chaos Labs public risk reports (for methodology ideas)
- Post-mortems: Black Thursday 2020 (MakerDAO), Cream, Euler, Mango Markets, Venus (XVS), Terra/UST
- *Leveraged Trading* style literature on liquidation cascades, and academic papers on "DeFi lending liquidations" and "fire sales"