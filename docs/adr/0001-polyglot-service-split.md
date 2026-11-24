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
