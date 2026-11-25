# ADR 0004: Reconstruct positions by "touch and read", not by replaying amounts

Date: 2026-11-25 | Status: accepted

## Context
Aave events carry underlying amounts (Supply 100 USDC), but positions are stored on-chain as SCALED balances
(amount / index, with ray rounding). Rebuilding scaled balances purely from event amounts means re-implementing
every rounding rule of every code path (supply, withdraw, borrow, repay, liquidation, aToken transfers, interest
mints/burns) and any 1-wei drift accumulates silently and forever.

## Decision
Events only tell us WHICH (asset, account) positions changed in a block. For each touched key we READ the exact
scaled supply, scaled variable debt and collateral flag from the chain at that block (one Multicall3 per block) and
store the result. Interest accrual needs no event: real balance = scaled x normalized index (state.rs ports Aave's
MathUtils so the engine can compute it at any timestamp).

## Consequences
+ Balances are exact by construction; no rounding drift; far less protocol logic to port.
+ Reorg safety is simple: a block's rows depend only on that block's reads (history table + rollback).
- Needs historical `eth_call` (archive state) for backfill, and one extra RPC per event block.
- Anything that changes a balance WITHOUT a decoded event is invisible until the next touch. Known gaps: see
  docs/PHASE1.md "Known gaps". `scripts/verify_state.py` (coverage check) exists to expose them.
- Starting mid-history only knows accounts touched since the start block (partial coverage, by design for Phase 1).
