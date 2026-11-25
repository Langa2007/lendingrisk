-- Phase 1: indexer state, history (for reorg rollback), token map. Idempotent.

-- Append-only history of every position version, keyed by block. `position` holds the latest.
CREATE TABLE IF NOT EXISTS position_history (
  protocol           text    NOT NULL,
  chain_id           bigint  NOT NULL,
  account            text    NOT NULL,
  asset              text    NOT NULL,
  block_number       bigint  NOT NULL,
  scaled_supply      numeric NOT NULL,
  scaled_debt        numeric NOT NULL,
  collateral_enabled boolean NOT NULL,
  PRIMARY KEY (protocol, chain_id, account, asset, block_number)
);
CREATE INDEX IF NOT EXISTS position_history_block_idx ON position_history (protocol, chain_id, block_number);

-- eMode category per account: latest + history
CREATE TABLE IF NOT EXISTS account_emode (
  protocol   text   NOT NULL,
  chain_id   bigint NOT NULL,
  account    text   NOT NULL,
  category   integer NOT NULL,
  last_block bigint NOT NULL,
  PRIMARY KEY (protocol, chain_id, account)
);
CREATE TABLE IF NOT EXISTS account_emode_history (
  protocol     text    NOT NULL,
  chain_id     bigint  NOT NULL,
  account      text    NOT NULL,
  block_number bigint  NOT NULL,
  category     integer NOT NULL,
  PRIMARY KEY (protocol, chain_id, account, block_number)
);
CREATE INDEX IF NOT EXISTS account_emode_history_block_idx ON account_emode_history (protocol, chain_id, block_number);

-- Reserve -> token addresses (refreshed by the indexer)
CREATE TABLE IF NOT EXISTS reserve_token (
  protocol            text    NOT NULL,
  chain_id            bigint  NOT NULL,
  asset               text    NOT NULL,
  atoken              text    NOT NULL,
  variable_debt_token text    NOT NULL,
  symbol              text    NOT NULL,
  decimals            integer NOT NULL,
  updated_block       bigint  NOT NULL,
  PRIMARY KEY (protocol, chain_id, asset)
);

-- Indexes that make rollback (delete everything above block N) cheap
CREATE INDEX IF NOT EXISTS reserve_state_block_idx ON reserve_state (protocol, chain_id, block_number);
CREATE INDEX IF NOT EXISTS liquidation_block_idx   ON liquidation (protocol, chain_id, block_number);
