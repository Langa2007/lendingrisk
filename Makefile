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
