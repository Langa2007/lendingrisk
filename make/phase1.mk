# Phase 1 targets. Add to the main Makefile with:   echo 'include make/phase1.mk' >> Makefile
# (uses the same .RECIPEPREFIX := > as the main Makefile)
.PHONY: indexer-test indexer-status indexer-backfill indexer-run verify-state mock-rpc phase1-check

indexer-test:
> TEST_DATABASE_URL=$${TEST_DATABASE_URL:-postgres://tahadhari:tahadhari@localhost:5433/tahadhari} cargo test -p indexer

indexer-status:
> cargo run -q -p indexer -- status

# make indexer-backfill FROM=21000000 [TO=21005000]
indexer-backfill:
> cargo run --release -p indexer -- backfill --from $(FROM) $(if $(TO),--to $(TO),)

# make indexer-run START=21000000   (START only needed on an empty database; later runs resume)
indexer-run:
> cargo run --release -p indexer -- run $(if $(START),--start-block $(START),)

verify-state:
> . engine/.venv/bin/activate && python scripts/verify_state.py $(or $(N),1000)

# local fake Aave node (no RPC key needed): see docs/PHASE1.md
mock-rpc:
> . engine/.venv/bin/activate && python scripts/mock_rpc.py

phase1-check: indexer-test
> cargo fmt --all -- --check
> cargo clippy --workspace --all-targets -- -D warnings
