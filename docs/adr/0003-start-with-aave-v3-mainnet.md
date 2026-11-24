# ADR 0003: Start with Aave v3 on Ethereum mainnet

Date: 2026-11-24 | Status: accepted

## Decision
First protocol and chain: Aave v3, Ethereum mainnet. Best documentation, richest data, and the most post-mortems to learn from. Other protocols (Compound v3, Morpho Blue) and L2s come in Phase 11 through the adapter abstraction.

## Consequences
Mainnet RPC calls and archive data cost more than L2s; development uses a local anvil fork to limit usage.
