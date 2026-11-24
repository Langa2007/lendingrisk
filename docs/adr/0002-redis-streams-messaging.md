# ADR 0002: Redis Streams for inter-service messaging

Date: 2026-11-24 | Status: accepted

## Context
Services in different languages need durable, replayable messaging. Plain Redis pub/sub loses messages when a consumer is down.

## Decision
Use Redis Streams with consumer groups, bounded with MAXLEN, and idempotent consumers keyed by message id or business key. Postgres remains the source of truth for history.

## Consequences
+ Restarted consumers resume where they left off; simple to run locally.
- Not a full log like Kafka: retention is bounded, so backtests read from Postgres/historical data, not from streams.
