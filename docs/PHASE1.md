# Phase 1: the Rust indexer

Goal: reconstruct every Aave v3 account's collateral and debt from chain data, correctly, restartably, and
reorg-safely. Exit criteria (from PHASES.md): sampled accounts match the chain, kill/restart causes no gaps or
duplicates, a simulated reorg leaves state correct. Tag `v0.1.0`.

## What exists

```
indexer/src/
  abi.rs       Aave event + view-call definitions (alloy sol! macro), Multicall3 address
  types.rs     plain data types (Header, RawLog, PositionRead, BlockWrite ...)
  decode.rs    AaveV3Adapter: logs -> touched positions, reserve updates, liquidations, oracle updates (pure, tested)
  state.rs     Aave interest-index math ported from MathUtils/WadRayMath (used by the Phase 2 engine)
  chain.rs     trait ChainSource: the blockchain as the indexer sees it
  rpc.rs       ChainSource over JSON-RPC: retry + provider failover, getLogs by range/hash, Multicall3 reads
  store.rs     trait Store + PgStore (PostgreSQL): atomic per-block writes, history, reorg rollback
  emit.rs      Redis Streams notification (schemas/json/block_processed.v1.json); never blocks indexing
  metrics.rs   /metrics endpoint (Prometheus text) on 127.0.0.1:9101
  indexer.rs   the engine: backfill, live tail, reorg detection/repair  (+ 20 scenario tests)
  testutil.rs  scripted fake chain (with reorgs) + in-memory store used by the tests
  main.rs      CLI: backfill | run | status
db/migrations/0002_indexer_state.sql   position_history, emode, reserve_token, rollback indexes
db/migrations/0003_stable_debt.sql     stable-debt columns in latest and historical positions
scripts/mock_rpc.py                    fake Aave node with a /admin/reorg switch (no RPC key needed)
scripts/verify_state.py                DB-vs-chain sample check + missed-event coverage check
docs/adr/0004-touch-and-read-state-reconstruction.md
```

### How it works (read this before reading the code)
1. **Events say WHAT changed, the chain says to WHAT.** For each block, the adapter turns MintUnbacked,
   Supply/Withdraw/Borrow/Repay/SwapBorrowRateMode/RebalanceStableBorrowRate/LiquidationCall,
   collateral-toggle/aToken-transfer logs into a set of (asset, account) "touches"; feed `AnswerUpdated` logs are
   stored and emitted separately as oracle updates.
   `rpc.rs` then reads the exact scaled balances for those touches at that block in one Multicall3 call.
   (ADR 0004 explains why this beats replaying amounts.)
2. **One block = one database transaction** (`Store::apply_block`): the block record, position history, latest
   positions, reserve indices, liquidations, eMode. Re-applying a block is harmless (idempotent upserts).
3. **Two modes.** Backfill works in chunks of blocks and only advances the cursor at the chunk end, so a crash
   mid-chunk redoes the chunk instead of skipping part of it. Live mode processes block by block, recording every
   block hash. The chunk size shrinks automatically if the provider rejects a range.
4. **Reorgs.** Each poll first checks that the block we last processed is still canonical, and each new block's
   parent hash is checked against what we stored. On mismatch it walks back to the common ancestor, deletes
   everything above it (rebuilding "latest" rows from history), and replays the new fork. A reorg deeper than
   `REORG_DEPTH` (64) stops with an error instead of guessing. A node that merely lags (same chain, lower head)
   does NOT trigger a rollback.

## Setup

```bash
cd ~/tahadhari
tar xzf ~/Downloads/phase1.tar.gz            # adds indexer/, make/, scripts/, docs/, db/migrations/0002...
echo 'include make/phase1.mk' >> Makefile     # adds the make targets below
rustc --version                               # need 1.88+ ; if older:  rustup update stable
make up && make migrate                       # applies 0001, 0002, and later migrations (idempotent)
source engine/.venv/bin/activate
pip install web3 "psycopg[binary]" eth_abi "eth-hash[pycryptodome]" eth_utils
```
`make migrate` runs every file in db/migrations/, so it is safe to run again.

## 1. Prove the code on your machine WITHOUT an RPC key

```bash
make indexer-test            # 39 tests (needs `make up` and TEST_DATABASE_URL for the 3 Postgres tests)
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
```

End-to-end against the fake Aave node (terminal A runs the fake node, terminal B the real indexer):

```bash
# terminal A
make mock-rpc                                   # fake chain with 40 blocks on http://127.0.0.1:8545

# terminal B
export RPC_URL=http://127.0.0.1:8545 CHAIN_ID=31337 DATABASE_URL=postgres://tahadhari:tahadhari@localhost:5433/tahadhari
cargo run -p indexer -- run --start-block 1     # catches up, then tails; leave running
# terminal C
make verify-state N=100                          # -> RESULT: PASS
curl -X POST localhost:8545/admin/reorg          # fork B replaces blocks 33-40, new head is 38 (shorter!)
# terminal B logs:  WARN REORG detected at our tip ... ancestor=32 depth=8
make verify-state N=100                          # -> RESULT: PASS (state now equals fork B)
curl -s localhost:9101/metrics | grep reorgs     # tahadhari_indexer_reorgs_total 1
```
Crash test: `kill -9` the indexer (`pkill -9 -x indexer`), start it again with `cargo run -p indexer -- run`
(no `--start-block`: it resumes from the stored cursor), run `make verify-state` again: identical state, no duplicates.
In my own run of exactly this flow the database matched the fake chain's ground truth before the reorg, after
the reorg, and after a crash and restart.

## 2. Run it on real Aave (Ethereum mainnet)

1. `.env` needs `RPC_URL` (Alchemy/Infura/QuickNode). Optional: `RPC_URL_BACKUP` for failover,
   `WS_RPC_URL` for `newHeads` notifications (polling remains the fallback), and
   `REDIS_URL=redis://localhost:6379` to emit `stream:block_processed` and `stream:oracle_update`.
   `RPC_RPS` controls per-provider rate limiting; `RPC_QUORUM` requires that many matching block headers.
   Backfill reads state at HISTORICAL blocks, so use a provider/plan that serves archive `eth_call`s for your range.
2. Pick a small recent window first (a day is ~7,200 blocks). Get the head: `make indexer-status` prints it
   once the database has a cursor; or `cast block-number --rpc-url $RPC_URL`.
3. ```bash
   make indexer-backfill FROM=<head-7200> TO=<head-100>     # confirmed blocks only (head - 12 by default)
   make indexer-run                                          # then tail forever; Ctrl-C to stop
   ```
   Fresh database shortcut: `make indexer-run START=<head-7200>` does both.
   To bootstrap full history, set `AAVE_V3_START_BLOCK` to the verified Aave deployment block for the selected
   chain and use an archive RPC provider. This scans from that block and can consume substantial RPC quota; test
   a bounded range first.
4. In another terminal: `make verify-state N=1000`. **Phase 1 exit criterion:** SAMPLE 1000/1000 and COVERAGE
   100%. Mismatches print the account, asset and both values; paste them to me.

Tuning (all via `.env` or flags): `CHUNK_SIZE` (getLogs range, shrinks on provider limits), `CONFIRMATIONS`,
`POLL_MS`, `REORG_DEPTH`, `METRICS_ADDR`, `CHAIN_ID`.

## Exit-criteria checklist
- [ ] `make verify-state N=1000` -> PASS on mainnet data (sample + coverage)
- [ ] kill -9 + restart -> `verify-state` still PASS, no duplicate history rows
- [ ] mock reorg (above) -> REORG logged, `verify-state` PASS
- [ ] `make phase1-check` clean (tests, fmt, clippy)
- [ ] `git add -A && git commit -m "feat(indexer): phase 1" && git tag v0.1.0`

## What was and was NOT tested (be honest in your write-up)
Tested here: all decoding and math (unit tests); indexer logic including restart, idempotency, backfill chunking,
provider range limits, unknown-reserve refresh, reorg repair incl. shorter-chain reorgs and lagging nodes (scripted
fake chain); the PostgreSQL SQL against a real PostgreSQL 16 server (without TimescaleDB); and the real binary
end-to-end through alloy against `mock_rpc.py` (backfill, live tail, reorg, crash/restart, verifier).
**Not tested:** a real Ethereum node or real Aave contracts (`mock_rpc.py` is MY model of Aave's interfaces, so a
real-chain surprise is possible: ABI differences in newer Aave versions, provider quirks); TimescaleDB hypertables
(the schema uses them; the SQL is standard but I could not run the extension); Redis emission (unit-tested message
format only); behaviour under real load.

## Known gaps (documented, not hidden)
- **Partial coverage unless bootstrapped from deployment:** only accounts touched since the start block are known.
  Full-history scanning is supported through `AAVE_V3_START_BLOCK`, but must be run with the correct deployment
  block and an archive RPC provider before claiming full coverage.
- Stable debt is stored separately from scaled variable debt; stable debt is an accrued on-chain balance.
- `MintedToTreasury` is not decoded directly; its aToken `BalanceTransfer` is used to touch the affected treasury
  account when emitted.
- **eMode category** is stored, but position-level eMode semantics are applied in Phase 2, not here.
- Polling remains the fallback when `WS_RPC_URL` is unset or its subscription closes.
- `RPC_RPS` rate-limits requests per provider. `RPC_QUORUM` requires matching block hashes for header reads;
  other reads still use retry/failover rather than a quorum.
- The adapter currently covers event decoding only; RPC state reads and protocol initialization remain Aave v3-specific.
- `processed_block` has no `protocol` column, so one indexer per chain until Phase 11 adds it.
- New reserves are picked up when an event for them is seen, and every 1,800 blocks (~6 h).

## Troubleshooting
| Symptom | Likely cause / fix |
|---|---|
| `error: package requires rustc 1.88` | `rustup update stable` |
| `get_logs failed ... range` / `query returned more than` | lower `CHUNK_SIZE` (it also auto-shrinks); some free tiers allow 10 blocks |
| `missing trie node` / `state not available` | provider lacks archive state for that block; use an archive plan or a more recent window |
| `call failed for <acct> on <asset>` | ABI mismatch with that Aave deployment: paste the full error to me |
| `store is empty: run backfill` | first run needs `--start-block` or `backfill --from` |
| `reorg deeper than REORG_DEPTH` | provider returned inconsistent data, or a real deep reorg; re-seed from an earlier block |
| `permission denied` / connection refused on 5433 | `make up`; check `DATABASE_URL` uses port **5433** |

## Rust concepts to study in this codebase (you want to master Rust)
Read in this order: `types.rs` (structs, derives) -> `state.rs` (integer math, `Option` + `checked_*`, `#[cfg(test)]`)
-> `decode.rs` (pattern matching, macros: `dec!`) -> `chain.rs`/`store.rs` (traits, `async_trait`, `Result`/`?`/`anyhow`)
-> `indexer.rs` (generics `Indexer<C: ChainSource, S: Store, E: Emitter>`, `tokio::select!`) -> `testutil.rs` (test
doubles, `Arc<Mutex<_>>`) -> `rpc.rs` (closures returning futures, retry/failover) -> `main.rs` (clap, tokio runtime).
Exercises: (1) add a `Metrics` counter for events per type; (2) add a quorum test for inconsistent providers;
(3) implement reconnecting WebSocket subscriptions; (4) extend the adapter boundary to protocol state reads.
