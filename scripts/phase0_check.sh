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
