#!/usr/bin/env bash
# Tahadhari - Phase 0 setup. Run from the repo root:
#   bash 'phase0 setup.sh'
# Safe to re-run: existing files are skipped unless you run  FORCE=1 bash 'phase0 setup.sh'
set -euo pipefail

if [ ! -f go.work ] || [ ! -f Cargo.toml ]; then
  echo "Run this from the tahadhari repo root (the folder with go.work and Cargo.toml)." >&2
  exit 1
fi

FORCE="${FORCE:-0}"

# put <path>: write stdin to <path> unless it already exists
put() {
  local path="$1"
  mkdir -p "$(dirname "$path")"
  if [ -e "$path" ] && [ "$FORCE" != "1" ]; then
    echo "  skip   $path (exists)"
    cat > /dev/null
  else
    cat > "$path"
    echo "  write  $path"
  fi
}

mkdir -p schemas/json docs/adr docs/methodology docs/runbooks db/migrations scripts \
         .github/workflows learning config/scenarios

echo "== infrastructure =="

put docker-compose.yml <<'EOF'
services:
  postgres:
    image: timescale/timescaledb:latest-pg16
    container_name: tahadhari-postgres
    environment:
      POSTGRES_USER: tahadhari
      POSTGRES_PASSWORD: tahadhari
      POSTGRES_DB: tahadhari
    ports:
      - "127.0.0.1:5433:5432"   # 5433 on the host so it never clashes with a local postgres
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U tahadhari -d tahadhari"]
      interval: 5s
      timeout: 5s
      retries: 10

  redis:
    image: redis:7-alpine
    container_name: tahadhari-redis
    command: ["redis-server", "--appendonly", "yes"]
    ports:
      - "127.0.0.1:6379:6379"
    volumes:
      - redisdata:/data
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 10

volumes:
  pgdata:
  redisdata:
EOF

put .env.example <<'EOF'
# Copy to .env and fill in. NEVER commit .env.
# Ethereum mainnet RPC (Alchemy / Infura / QuickNode free tier is enough for Phase 0)
RPC_URL=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY

# Optional second provider for failover later
RPC_URL_BACKUP=

# Aave v3 PoolAddressesProvider (Ethereum mainnet). Verify against Aave's official address book.
AAVE_V3_ADDRESSES_PROVIDER=0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e

# eth_getLogs range per request. Free tiers often allow only 10-2000 blocks; lower this if you get errors.
LOG_CHUNK=500

DATABASE_URL=postgres://tahadhari:tahadhari@localhost:5433/tahadhari
REDIS_URL=redis://localhost:6379

# Alerts (Phase 6)
TELEGRAM_BOT_TOKEN=
TELEGRAM_CHAT_ID=
EOF

put Makefile <<'EOF'
.RECIPEPREFIX := >
COMPOSE ?= docker compose

.PHONY: up down logs psql migrate test lint fork check

up:
> $(COMPOSE) up -d

down:
> $(COMPOSE) down

logs:
> $(COMPOSE) logs -f --tail=100

psql:
> $(COMPOSE) exec postgres psql -U tahadhari -d tahadhari

migrate:
> for f in db/migrations/*.sql; do echo "applying $$f"; $(COMPOSE) exec -T postgres psql -v ON_ERROR_STOP=1 -U tahadhari -d tahadhari < $$f || exit 1; done

test:
> cargo test --workspace
> for m in alerts api keeper cli; do (cd $$m && go test -race ./...) || exit 1; done
> cd engine && . .venv/bin/activate && pytest -q
> cd 01-solidity-basics && forge test
> if [ -f learning/02-erc20-vault/foundry.toml ]; then cd learning/02-erc20-vault && forge test; else echo "Foundry vault project not initialized"; fi

lint:
> cargo fmt --all -- --check
> cargo clippy --workspace -- -D warnings
> for m in alerts api keeper cli; do (cd $$m && go vet ./...) || exit 1; done
> cd engine && . .venv/bin/activate && ruff check .
> cd 01-solidity-basics && forge fmt --check
> if [ -f learning/02-erc20-vault/foundry.toml ]; then cd learning/02-erc20-vault && forge fmt --check; fi

fork:
> set -a; . ./.env; set +a; anvil --fork-url "$$RPC_URL"

check:
> bash scripts/phase0_check.sh
EOF

put db/migrations/0001_init.sql <<'EOF'
-- Tahadhari initial schema. Idempotent: safe to run more than once.
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Blocks the indexer has processed (reorg detection uses hash + parent_hash)
CREATE TABLE IF NOT EXISTS processed_block (
  chain_id     bigint      NOT NULL,
  block_number bigint      NOT NULL,
  block_hash   text        NOT NULL,
  parent_hash  text        NOT NULL,
  ts           timestamptz NOT NULL,
  PRIMARY KEY (chain_id, block_number)
);

-- Reserve state over time (hypertable)
CREATE TABLE IF NOT EXISTS reserve_state (
  protocol        text             NOT NULL,
  chain_id        bigint           NOT NULL,
  asset           text             NOT NULL,
  block_number    bigint           NOT NULL,
  block_hash      text             NOT NULL,
  ts              timestamptz      NOT NULL,
  liquidity_index numeric          NOT NULL,
  borrow_index    numeric          NOT NULL,
  total_supply    numeric,
  total_borrow    numeric,
  utilization     double precision,
  borrow_rate     numeric,
  supply_rate     numeric,
  ltv             integer,
  liq_threshold   integer,
  liq_bonus       integer,
  supply_cap      numeric,
  borrow_cap      numeric,
  is_frozen       boolean,
  is_paused       boolean,
  PRIMARY KEY (protocol, chain_id, asset, block_number, ts)
);
SELECT create_hypertable('reserve_state', 'ts', if_not_exists => TRUE);

-- Current position per account and asset (scaled balances, as stored on-chain)
CREATE TABLE IF NOT EXISTS position (
  protocol           text    NOT NULL,
  chain_id           bigint  NOT NULL,
  account            text    NOT NULL,
  asset              text    NOT NULL,
  scaled_supply      numeric NOT NULL DEFAULT 0,
  scaled_debt        numeric NOT NULL DEFAULT 0,
  collateral_enabled boolean NOT NULL DEFAULT false,
  last_block         bigint  NOT NULL,
  PRIMARY KEY (protocol, chain_id, account, asset)
);
CREATE INDEX IF NOT EXISTS position_debt_idx ON position (protocol, chain_id) WHERE scaled_debt > 0;

-- Oracle updates (hypertable)
CREATE TABLE IF NOT EXISTS oracle_update (
  chain_id     bigint      NOT NULL,
  feed         text        NOT NULL,
  block_number bigint      NOT NULL,
  round_id     numeric     NOT NULL,
  answer       numeric     NOT NULL,
  updated_at   timestamptz NOT NULL,
  ts           timestamptz NOT NULL,
  PRIMARY KEY (chain_id, feed, block_number, round_id, ts)
);
SELECT create_hypertable('oracle_update', 'ts', if_not_exists => TRUE);

-- Liquidations (hypertable)
CREATE TABLE IF NOT EXISTS liquidation (
  protocol          text        NOT NULL,
  chain_id          bigint      NOT NULL,
  block_number      bigint      NOT NULL,
  tx_hash           text        NOT NULL,
  log_index         integer     NOT NULL,
  account           text        NOT NULL,
  collateral_asset  text        NOT NULL,
  debt_asset        text        NOT NULL,
  debt_repaid       numeric     NOT NULL,
  collateral_seized numeric     NOT NULL,
  liquidator        text        NOT NULL,
  gas_price_wei     numeric,
  ts                timestamptz NOT NULL,
  PRIMARY KEY (chain_id, tx_hash, log_index, ts)
);
SELECT create_hypertable('liquidation', 'ts', if_not_exists => TRUE);

-- Computed metrics (hypertable)
CREATE TABLE IF NOT EXISTS risk_snapshot (
  ts          timestamptz      NOT NULL,
  protocol    text             NOT NULL,
  chain_id    bigint           NOT NULL,
  metric      text             NOT NULL,
  params      jsonb            NOT NULL DEFAULT '{}'::jsonb,
  value       double precision NOT NULL,
  block_number bigint
);
SELECT create_hypertable('risk_snapshot', 'ts', if_not_exists => TRUE);
CREATE INDEX IF NOT EXISTS risk_snapshot_metric_idx ON risk_snapshot (protocol, metric, ts DESC);

-- Alert history
CREATE TABLE IF NOT EXISTS alert (
  id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
  ts           timestamptz NOT NULL DEFAULT now(),
  rule_id      text        NOT NULL,
  severity     text        NOT NULL,
  status       text        NOT NULL,       -- firing | resolved
  metric_value double precision,
  threshold    text,
  payload      jsonb       NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS alert_rule_ts_idx ON alert (rule_id, ts DESC);
EOF

echo "== message schemas =="

put schemas/README.md <<'EOF'
# Message schemas

The contract between the Rust indexer, Python engine, and Go services. JSON Schema in `json/` is the source of truth.

Rules
- Every message has `schema_version`, `trace_id`, `protocol`, `chain_id`.
- Add fields freely. Never repurpose or remove fields inside a version. Breaking change = new `.v2` file.
- Transport: Redis Streams with consumer groups (not pub/sub). Consumers must be idempotent.
- Time: block timestamps for logic, wall clock for ops metrics only.

Streams (Phase 0 defines the first three; the rest come with their phases)

| Stream | Producer | Consumers | Schema |
|---|---|---|---|
| `stream:block_processed` | indexer (Rust) | engine, alerts | block_processed.v1 |
| `stream:oracle_update`   | indexer (Rust) | engine, alerts | oracle_update.v1 |
| `stream:metric`          | engine (Python) | alerts, api (Go) | metric.v1 |
EOF

put schemas/json/block_processed.v1.json <<'EOF'
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "block_processed.v1",
  "type": "object",
  "required": ["schema_version", "trace_id", "protocol", "chain_id", "block_number", "block_hash", "ts"],
  "properties": {
    "schema_version": { "const": 1 },
    "trace_id": { "type": "string" },
    "protocol": { "type": "string" },
    "chain_id": { "type": "integer" },
    "block_number": { "type": "integer", "minimum": 0 },
    "block_hash": { "type": "string" },
    "ts": { "type": "string", "format": "date-time" }
  },
  "additionalProperties": true
}
EOF

put schemas/json/oracle_update.v1.json <<'EOF'
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "oracle_update.v1",
  "type": "object",
  "required": ["schema_version", "trace_id", "chain_id", "feed", "block_number", "round_id", "answer", "updated_at"],
  "properties": {
    "schema_version": { "const": 1 },
    "trace_id": { "type": "string" },
    "chain_id": { "type": "integer" },
    "feed": { "type": "string" },
    "block_number": { "type": "integer", "minimum": 0 },
    "round_id": { "type": "string" },
    "answer": { "type": "string", "description": "integer as decimal string; scale by feed decimals" },
    "decimals": { "type": "integer" },
    "updated_at": { "type": "string", "format": "date-time" }
  },
  "additionalProperties": true
}
EOF

put schemas/json/metric.v1.json <<'EOF'
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "metric.v1",
  "type": "object",
  "required": ["schema_version", "trace_id", "protocol", "chain_id", "block_number", "ts", "metric", "value"],
  "properties": {
    "schema_version": { "const": 1 },
    "trace_id": { "type": "string" },
    "protocol": { "type": "string" },
    "chain_id": { "type": "integer" },
    "block_number": { "type": "integer", "minimum": 0 },
    "ts": { "type": "string", "format": "date-time" },
    "metric": { "type": "string" },
    "params": { "type": "object" },
    "value": { "type": "number" },
    "unit": { "type": "string" },
    "assumptions_hash": { "type": "string" }
  },
  "additionalProperties": true
}
EOF

echo "== architecture decision records =="

put docs/adr/0001-polyglot-service-split.md <<'EOF'
# ADR 0001: Polyglot service split

Date: 2026-11-24 | Status: accepted

## Context
The project has four distinct workloads: exact on-chain state reconstruction, numerical risk modeling, concurrent network services (alerts, API, bots), and contract testing. I also want hands-on depth in Rust, Go, and Python, and I am new to Solidity.

## Decision
- Rust: indexer (integer-exact state, reorg handling).
- Python: risk engine, backtests, simulation, optimizer.
- Go: alert service, API/WebSocket gateway, keeper, ops CLI.
- Solidity + Foundry: mock pool and attack lab.
- Services communicate only through Postgres/TimescaleDB, Redis Streams, and versioned JSON schemas in `schemas/`. No cross-language imports.

## Consequences
+ Each language does what it is best at; any service can be rewritten independently.
- Four toolchains and CI jobs; schema drift is a risk, mitigated by generating types from `schemas/` in CI.
- Rust indexer is the steepest learning curve; a small Python reference implementation is the fallback.
EOF

put docs/adr/0002-redis-streams-messaging.md <<'EOF'
# ADR 0002: Redis Streams for inter-service messaging

Date: 2026-11-24 | Status: accepted

## Context
Services in different languages need durable, replayable messaging. Plain Redis pub/sub loses messages when a consumer is down.

## Decision
Use Redis Streams with consumer groups, bounded with MAXLEN, and idempotent consumers keyed by message id or business key. Postgres remains the source of truth for history.

## Consequences
+ Restarted consumers resume where they left off; simple to run locally.
- Not a full log like Kafka: retention is bounded, so backtests read from Postgres/historical data, not from streams.
EOF

put docs/adr/0003-start-with-aave-v3-mainnet.md <<'EOF'
# ADR 0003: Start with Aave v3 on Ethereum mainnet

Date: 2026-11-24 | Status: accepted

## Decision
First protocol and chain: Aave v3, Ethereum mainnet. Best documentation, richest data, and the most post-mortems to learn from. Other protocols (Compound v3, Morpho Blue) and L2s come in Phase 11 through the adapter abstraction.

## Consequences
Mainnet RPC calls and archive data cost more than L2s; development uses a local anvil fork to limit usage.
EOF

echo "== docs templates (you fill these in by hand) =="

put docs/methodology/health-factor.md <<'EOF'
# Health factor: methodology

## Definition
HF = ( sum over collateral assets of  balance_i * price_i * LT_i ) / ( sum over debt assets of  debt_j * price_j )

- Only assets with `usageAsCollateralEnabled` and LT > 0 count as collateral.
- Balances include accrued interest (scaled balance x index).
- Prices come from the protocol oracle (USD, 8 decimals on Aave v3).
- HF < 1 means the position can be liquidated.

## Worked example (fill in from scripts/verify_aave_hf.py output)
| Account | Block | Collateral USD (weighted by LT) | Debt USD | My HF | Protocol HF | Diff |
|---|---|---|---|---|---|---|
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |
| 0x... | | | | | | |

## Things I learned / surprises
- (eMode accounts use category LT and price source: why my number was off)
- (rounding differences)
- (which accounts I skipped and why)

## Open questions
- 
EOF

put learning/04-reading-aave-notes.md <<'EOF'
# Reading Aave v3 contracts: notes

Repo: github.com/aave-dao/aave-v3-origin (or aave/aave-v3-core for the classic layout). Read in this order and answer the questions in your own words.

1. `contracts/protocol/libraries/logic/GenericLogic.sol` -> `calculateUserAccountData`
   - How is the weighted average liquidation threshold computed? Where does the rounding happen?
   - How does eMode change the LT and the price source?
2. `contracts/protocol/libraries/logic/ReserveLogic.sol` -> `getNormalizedIncome`, `getNormalizedDebt`
   - How do the indices grow between events? Why does debt change with no event emitted?
3. `contracts/protocol/libraries/logic/ValidationLogic.sol` -> `validateLiquidationCall`
   - Which conditions make a liquidation valid? What is the HF threshold for the 100% close factor?
4. `contracts/protocol/libraries/logic/LiquidationLogic.sol` -> `executeLiquidationCall`
   - How are bonus and close factor applied? What happens when collateral is smaller than debt x (1 + bonus)?
5. `contracts/protocol/libraries/math/WadRayMath.sol` and `PercentageMath.sol`
   - What do wad (1e18) and ray (1e27) mean? Which direction does each rounding function round?

## My answers
(write here)

## Events the indexer must decode (list them as you find them)
- Supply, Withdraw, Borrow, Repay, LiquidationCall, ReserveDataUpdated, ReserveUsedAsCollateralEnabled/Disabled, UserEModeSet
EOF

echo "== CI skeletons =="

put .github/workflows/ci-rust.yml <<'EOF'
name: ci-rust
on: [push, pull_request]
jobs:
  rust:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: "rustfmt, clippy" }
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace -- -D warnings
      - run: cargo test --workspace
EOF

put .github/workflows/ci-go.yml <<'EOF'
name: ci-go
on: [push, pull_request]
jobs:
  go:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        module: [alerts, api, keeper, cli]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-go@v5
        with: { go-version: "stable" }
      - run: cd ${{ matrix.module }} && go vet ./... && go test -race ./...
EOF

put .github/workflows/ci-python.yml <<'EOF'
name: ci-python
on: [push, pull_request]
jobs:
  python:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with: { python-version: "3.12" }
      - run: cd engine && pip install -e ".[dev]" && ruff check . && pytest -q
EOF

put .github/workflows/ci-contracts.yml <<'EOF'
name: ci-contracts
on: [push, pull_request]
jobs:
  foundry:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: foundry-rs/foundry-toolchain@v1
      - run: cd 01-solidity-basics && forge build && forge test -vv
      - run: if [ -f learning/02-erc20-vault/foundry.toml ]; then cd learning/02-erc20-vault && forge build && forge test -vv; fi
EOF

echo "== python: Aave health-factor exercise =="

put scripts/verify_aave_hf.py <<'EOF'
#!/usr/bin/env python3
"""
Phase 0 exercise: recompute Aave v3 health factors BY HAND and compare with the protocol.

Setup (inside the engine venv):   pip install web3
Run (from repo root):             python scripts/verify_aave_hf.py            # 5 accounts
                                  python scripts/verify_aave_hf.py 10         # 10 accounts

Needs RPC_URL in .env (see .env.example). Read-only: it only makes eth_call / eth_getLogs requests.
"""
import os
import sys
from pathlib import Path

from web3 import Web3


def load_env(path: Path) -> None:
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        os.environ.setdefault(k.strip(), v.strip())


load_env(Path(__file__).resolve().parent.parent / ".env")

RPC_URL = os.environ.get("RPC_URL", "")
PROVIDER = os.environ.get("AAVE_V3_ADDRESSES_PROVIDER", "0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e")
LOG_CHUNK = int(os.environ.get("LOG_CHUNK", "500"))
MAX_CHUNKS = 400
TOLERANCE = 1e-4  # relative difference allowed (rounding in the protocol's integer math)
N_ACCOUNTS = int(sys.argv[1]) if len(sys.argv) > 1 else 5


def fn(name, ins, outs):
    return {
        "type": "function",
        "name": name,
        "stateMutability": "view",
        "inputs": [{"name": f"i{k}", "type": t} for k, t in enumerate(ins)],
        "outputs": [{"name": f"o{k}", "type": t} for k, t in enumerate(outs)],
    }


PROVIDER_ABI = [
    fn("getPool", [], ["address"]),
    fn("getPoolDataProvider", [], ["address"]),
    fn("getPriceOracle", [], ["address"]),
]
POOL_ABI = [
    fn("getUserAccountData", ["address"], ["uint256"] * 6),
    fn("getReservesList", [], ["address[]"]),
    fn("getUserEMode", ["address"], ["uint256"]),
]
DATA_PROVIDER_ABI = [
    fn(
        "getReserveConfigurationData",
        ["address"],
        ["uint256", "uint256", "uint256", "uint256", "uint256", "bool", "bool", "bool", "bool", "bool"],
    ),
    fn(
        "getUserReserveData",
        ["address", "address"],
        ["uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint256", "uint40", "bool"],
    ),
]
ORACLE_ABI = [fn("getAssetPrice", ["address"], ["uint256"])]
ERC20_ABI = [fn("symbol", [], ["string"])]


def main() -> int:
    if not RPC_URL or "YOUR_KEY" in RPC_URL:
        print("Set RPC_URL in .env first (copy .env.example).")
        return 2

    w3 = Web3(Web3.HTTPProvider(RPC_URL, request_kwargs={"timeout": 30}))
    chain_id = w3.eth.chain_id
    if chain_id != 1:
        print(f"Warning: chain id is {chain_id}, expected 1 (Ethereum mainnet).")
    block = w3.eth.block_number - 2  # pin every call to ONE block so numbers are comparable
    print(f"chain {chain_id}, pinned block {block}")

    provider = w3.eth.contract(address=Web3.to_checksum_address(PROVIDER), abi=PROVIDER_ABI)
    pool_addr = provider.functions.getPool().call(block_identifier=block)
    dp_addr = provider.functions.getPoolDataProvider().call(block_identifier=block)
    oracle_addr = provider.functions.getPriceOracle().call(block_identifier=block)
    print(f"pool {pool_addr}\ndata provider {dp_addr}\noracle {oracle_addr}\n")

    pool = w3.eth.contract(address=pool_addr, abi=POOL_ABI)
    dp = w3.eth.contract(address=dp_addr, abi=DATA_PROVIDER_ABI)
    oracle = w3.eth.contract(address=oracle_addr, abi=ORACLE_ABI)

    reserves = pool.functions.getReservesList().call(block_identifier=block)
    print(f"{len(reserves)} reserves listed")

    sym_cache, cfg_cache, price_cache = {}, {}, {}

    def symbol(asset):
        if asset not in sym_cache:
            try:
                c = w3.eth.contract(address=asset, abi=ERC20_ABI)
                sym_cache[asset] = c.functions.symbol().call(block_identifier=block)
            except Exception:
                sym_cache[asset] = asset[:8]
        return sym_cache[asset]

    def cfg(asset):
        if asset not in cfg_cache:
            c = dp.functions.getReserveConfigurationData(asset).call(block_identifier=block)
            cfg_cache[asset] = {"decimals": c[0], "lt_bps": c[2], "active": c[8]}
        return cfg_cache[asset]

    def price(asset):  # USD, 8 decimals
        if asset not in price_cache:
            price_cache[asset] = oracle.functions.getAssetPrice(asset).call(block_identifier=block)
        return price_cache[asset]

    # ---- find accounts that recently borrowed (position owner = onBehalfOf, topic index 2) ----
    sig = "Borrow(address,address,address,uint256,uint8,uint256,uint16)"
    topic0 = "0x" + bytes(Web3.keccak(text=sig)).hex()
    users, seen = [], set()
    hi = block
    for _ in range(MAX_CHUNKS):
        lo = max(0, hi - LOG_CHUNK + 1)
        try:
            logs = w3.eth.get_logs({"address": pool_addr, "fromBlock": lo, "toBlock": hi, "topics": [topic0]})
        except Exception as e:
            print(f"get_logs failed for {lo}-{hi}: {e}\nLower LOG_CHUNK in .env and retry.")
            return 2
        for lg in reversed(logs):
            addr = Web3.to_checksum_address(bytes(lg["topics"][2])[-20:])
            if addr not in seen:
                seen.add(addr)
                users.append(addr)
        if len(users) >= N_ACCOUNTS * 6:
            break
        hi = lo - 1
    print(f"{len(users)} candidate borrowers found\n")

    done = ok = skipped = 0
    for user in users:
        if done >= N_ACCOUNTS:
            break
        acct = pool.functions.getUserAccountData(user).call(block_identifier=block)
        total_debt_base = acct[1]
        if total_debt_base == 0:
            continue  # already repaid
        emode = pool.functions.getUserEMode(user).call(block_identifier=block)
        if emode != 0:
            skipped += 1
            print(f"{user}  eMode category {emode}: skipped (eMode changes LT and prices; see learning notes)")
            continue

        weighted_collateral = 0.0
        debt = 0.0
        rows = []
        for asset in reserves:
            u = dp.functions.getUserReserveData(asset, user).call(block_identifier=block)
            a_bal, stable_debt, var_debt, use_as_coll = u[0], u[1], u[2], u[8]
            if a_bal == 0 and stable_debt == 0 and var_debt == 0:
                continue
            c = cfg(asset)
            p = price(asset) / 1e8
            dec = 10 ** c["decimals"]
            if a_bal and use_as_coll and c["lt_bps"] > 0 and c["active"]:
                v = a_bal / dec * p
                weighted_collateral += v * c["lt_bps"] / 10000
                rows.append(f"    collateral {symbol(asset):>8}  {a_bal/dec:>18.6f}  x ${p:,.2f}  x LT {c['lt_bps']/100:.2f}%")
            d = (stable_debt + var_debt) / dec
            if d:
                debt += d * p
                rows.append(f"    debt       {symbol(asset):>8}  {d:>18.6f}  x ${p:,.2f}")

        my_hf = weighted_collateral / debt if debt else float("inf")
        protocol_hf = acct[5] / 1e18
        diff = abs(my_hf - protocol_hf) / protocol_hf if protocol_hf else float("inf")
        good = diff < TOLERANCE
        done += 1
        ok += good
        print(f"{user}")
        print("\n".join(rows))
        print(f"    weighted collateral ${weighted_collateral:,.2f} / debt ${debt:,.2f}")
        print(f"    my HF {my_hf:.6f}   protocol HF {protocol_hf:.6f}   rel diff {diff:.2e}   {'PASS' if good else 'FAIL'}\n")

    print(f"{ok}/{done} accounts matched within {TOLERANCE:.0e} ({skipped} eMode accounts skipped)")
    if done == 0:
        print("No accounts compared. Try again, or raise N.")
        return 1
    return 0 if ok == done else 1


if __name__ == "__main__":
    sys.exit(main())
EOF

echo "== solidity: ERC-20 + vault exercise =="

if command -v forge >/dev/null 2>&1; then
  if [ ! -d learning/02-erc20-vault/src ]; then
    (cd learning && forge init 02-erc20-vault --no-git >/dev/null && \
      rm -f 02-erc20-vault/src/Counter.sol 02-erc20-vault/test/Counter.t.sol 02-erc20-vault/script/Counter.s.sol)
    echo "  forge init learning/02-erc20-vault"
  fi
else
  echo "  forge not found in PATH: skipping Solidity scaffold (fix PATH, then re-run this script)"
fi

if [ -d learning/02-erc20-vault/src ]; then

put learning/02-erc20-vault/src/SimpleToken.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @notice Minimal ERC-20 written by hand for learning. `mint` is unrestricted ON PURPOSE
///         (test token only). Never deploy something like this with real value.
contract SimpleToken {
    string public name;
    string public symbol;
    uint8 public constant decimals = 18;
    uint256 public totalSupply;

    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    error InsufficientBalance();
    error InsufficientAllowance();

    constructor(string memory name_, string memory symbol_) {
        name = name_;
        symbol = symbol_;
    }

    function mint(address to, uint256 amount) external {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            if (allowed < amount) revert InsufficientAllowance();
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        if (balanceOf[from] < amount) revert InsufficientBalance();
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}
EOF

put learning/02-erc20-vault/src/Vault.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20Like {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address who) external view returns (uint256);
}

/// @notice 1:1 deposit/withdraw vault. Note the order in withdraw(): checks, then effects, then
///         the external call (checks-effects-interactions).
contract Vault {
    IERC20Like public immutable token;
    mapping(address => uint256) public balances;
    uint256 public totalDeposits;

    event Deposited(address indexed user, uint256 amount);
    event Withdrawn(address indexed user, uint256 amount);

    error InsufficientBalance();
    error TransferFailed();

    constructor(IERC20Like token_) {
        token = token_;
    }

    function deposit(uint256 amount) external {
        balances[msg.sender] += amount;
        totalDeposits += amount;
        if (!token.transferFrom(msg.sender, address(this), amount)) revert TransferFailed();
        emit Deposited(msg.sender, amount);
    }

    function withdraw(uint256 amount) external {
        if (balances[msg.sender] < amount) revert InsufficientBalance();
        balances[msg.sender] -= amount;
        totalDeposits -= amount;
        if (!token.transfer(msg.sender, amount)) revert TransferFailed();
        emit Withdrawn(msg.sender, amount);
    }
}
EOF

put learning/02-erc20-vault/test/SimpleToken.t.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SimpleToken} from "../src/SimpleToken.sol";

contract SimpleTokenTest is Test {
    SimpleToken token;
    address alice = address(0xA11CE);
    address bob = address(0xB0B);

    function setUp() public {
        token = new SimpleToken("Test", "TST");
        token.mint(alice, 1000 ether);
    }

    function test_Transfer() public {
        vm.prank(alice);
        token.transfer(bob, 100 ether);
        assertEq(token.balanceOf(alice), 900 ether);
        assertEq(token.balanceOf(bob), 100 ether);
    }

    function test_TransferRevertsOnInsufficientBalance() public {
        vm.prank(alice);
        vm.expectRevert(SimpleToken.InsufficientBalance.selector);
        token.transfer(bob, 1001 ether);
    }

    function test_TransferFromNeedsAllowance() public {
        vm.prank(bob);
        vm.expectRevert(SimpleToken.InsufficientAllowance.selector);
        token.transferFrom(alice, bob, 1 ether);

        vm.prank(alice);
        token.approve(bob, 10 ether);
        vm.prank(bob);
        token.transferFrom(alice, bob, 10 ether);
        assertEq(token.balanceOf(bob), 10 ether);
        assertEq(token.allowance(alice, bob), 0);
    }

    /// Fuzz: for any recipient and any amount up to the balance, supply is conserved.
    function testFuzz_Transfer(address to, uint256 amount) public {
        vm.assume(to != address(0) && to != alice);
        amount = bound(amount, 0, 1000 ether);
        uint256 before = token.balanceOf(to);

        vm.prank(alice);
        token.transfer(to, amount);

        assertEq(token.balanceOf(alice), 1000 ether - amount);
        assertEq(token.balanceOf(to), before + amount);
        assertEq(token.totalSupply(), 1000 ether);
    }
}
EOF

put learning/02-erc20-vault/test/Vault.t.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SimpleToken} from "../src/SimpleToken.sol";
import {Vault, IERC20Like} from "../src/Vault.sol";

contract VaultTest is Test {
    SimpleToken token;
    Vault vault;
    address alice = address(0xA11CE);

    function setUp() public {
        token = new SimpleToken("Test", "TST");
        vault = new Vault(IERC20Like(address(token)));
        token.mint(alice, 1000 ether);
        vm.prank(alice);
        token.approve(address(vault), type(uint256).max);
    }

    function test_DepositAndWithdraw() public {
        vm.startPrank(alice);
        vault.deposit(100 ether);
        assertEq(vault.balances(alice), 100 ether);
        assertEq(token.balanceOf(address(vault)), 100 ether);

        vault.withdraw(40 ether);
        vm.stopPrank();

        assertEq(vault.balances(alice), 60 ether);
        assertEq(vault.totalDeposits(), 60 ether);
        assertEq(token.balanceOf(alice), 940 ether);
    }

    function test_WithdrawMoreThanBalanceReverts() public {
        vm.startPrank(alice);
        vault.deposit(10 ether);
        vm.expectRevert(Vault.InsufficientBalance.selector);
        vault.withdraw(11 ether);
        vm.stopPrank();
    }

    function testFuzz_DepositWithdrawRoundTrip(uint256 amount) public {
        amount = bound(amount, 0, 1000 ether);
        vm.startPrank(alice);
        vault.deposit(amount);
        vault.withdraw(amount);
        vm.stopPrank();
        assertEq(token.balanceOf(alice), 1000 ether);
        assertEq(vault.totalDeposits(), 0);
    }
}
EOF

put learning/02-erc20-vault/test/Vault.invariant.t.sol <<'EOF'
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SimpleToken} from "../src/SimpleToken.sol";
import {Vault, IERC20Like} from "../src/Vault.sol";

/// The handler constrains what the fuzzer can do, so calls are meaningful (valid actors, bounded amounts).
contract VaultHandler is Test {
    Vault public vault;
    SimpleToken public token;
    address[] public actors;
    uint256 public ghostDeposited;
    uint256 public ghostWithdrawn;

    constructor(Vault vault_, SimpleToken token_) {
        vault = vault_;
        token = token_;
        for (uint160 i = 0; i < 3; i++) {
            actors.push(address(uint160(0xA11CE) + i));
        }
    }

    function deposit(uint256 actorSeed, uint256 amount) external {
        address a = actors[actorSeed % actors.length];
        amount = bound(amount, 0, 1_000 ether);
        token.mint(a, amount);
        vm.startPrank(a);
        token.approve(address(vault), amount);
        vault.deposit(amount);
        vm.stopPrank();
        ghostDeposited += amount;
    }

    function withdraw(uint256 actorSeed, uint256 amount) external {
        address a = actors[actorSeed % actors.length];
        amount = bound(amount, 0, vault.balances(a));
        vm.prank(a);
        vault.withdraw(amount);
        ghostWithdrawn += amount;
    }
}

contract VaultInvariantTest is Test {
    SimpleToken token;
    Vault vault;
    VaultHandler handler;

    function setUp() public {
        token = new SimpleToken("Test", "TST");
        vault = new Vault(IERC20Like(address(token)));
        handler = new VaultHandler(vault, token);
        targetContract(address(handler));
    }

    /// The vault's token balance must always equal its internal accounting.
    function invariant_TokenBalanceMatchesAccounting() public view {
        assertEq(token.balanceOf(address(vault)), vault.totalDeposits());
    }

    /// Everything deposited minus everything withdrawn equals what the vault says it holds.
    function invariant_GhostVariablesMatch() public view {
        assertEq(handler.ghostDeposited() - handler.ghostWithdrawn(), vault.totalDeposits());
    }
}
EOF

fi

echo "== phase 0 exit-criteria checker =="

put scripts/phase0_check.sh <<'EOF'
#!/usr/bin/env bash
# Verifies the automatable Phase 0 exit criteria. Run from repo root:  bash scripts/phase0_check.sh
set -uo pipefail
pass=0; fail=0
check() { # check "<name>" <command...>
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then echo "PASS  $name"; pass=$((pass+1)); else echo "FAIL  $name"; fail=$((fail+1)); fi
}

COMPOSE="docker compose"; $COMPOSE version >/dev/null 2>&1 || COMPOSE="docker-compose"

check "cargo test (rust workspace)"        cargo test --workspace
for m in alerts api keeper cli; do
  check "go vet+test: $m"                  bash -c "cd $m && go vet ./... && go test ./..."
done
check "pytest (engine)"                    bash -c "cd engine && . .venv/bin/activate && pytest -q"
check "forge test (Solidity basics)"       bash -c "cd 01-solidity-basics && forge test"
check "forge test (learning vault + fuzz + invariant)" bash -c "cd learning/02-erc20-vault && forge test"
check "postgres container healthy"        bash -c "$COMPOSE ps postgres | grep -qi healthy"
check "redis container healthy"            bash -c "$COMPOSE ps redis | grep -qi healthy"
check "redis answers PING"                 bash -c "$COMPOSE exec -T redis redis-cli ping | grep -q PONG"
check "migration applied (hypertables)"    bash -c "$COMPOSE exec -T postgres psql -U tahadhari -d tahadhari -tAc \"select count(*) from timescaledb_information.hypertables\" | grep -qE '^[4-9]'"
for f in docs/adr/0001-polyglot-service-split.md docs/adr/0002-redis-streams-messaging.md schemas/json/metric.v1.json; do
  check "file exists: $f"                  test -s "$f"
done

echo
echo "passed: $pass   failed: $fail"
echo "MANUAL: python scripts/verify_aave_hf.py  -> all accounts PASS (hand-computed HF == protocol HF)"
echo "MANUAL: fill in the table in docs/methodology/health-factor.md with your 5 accounts"
[ "$fail" -eq 0 ]
EOF

chmod +x scripts/verify_aave_hf.py scripts/phase0_check.sh 2>/dev/null || true

echo
echo "Phase 0 files written. Next steps are in the chat message."
