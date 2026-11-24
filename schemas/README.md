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
