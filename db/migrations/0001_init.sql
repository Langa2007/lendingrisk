-- Tahadhari initial schema. Idempotent: safe to run more than once.
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Blocks the indexer has processed (reorg detection uses hash + parent_hash)
CREATE TABLE IF NOT EXISTS processed_block (
  chain_id     bigint      NOT NULL,
  block_number bigint      NOT NULL,
  block_hash   text        NOT NULL,
  parent_hash  text        NOT NULL,
  ts           timestamptz NOT NULL,
  PRIMARY KEY (chain_id, block_number)
);

-- Reserve state over time (hypertable)
CREATE TABLE IF NOT EXISTS reserve_state (
  protocol        text             NOT NULL,
  chain_id        bigint           NOT NULL,
  asset           text             NOT NULL,
  block_number    bigint           NOT NULL,
  block_hash      text             NOT NULL,
  ts              timestamptz      NOT NULL,
  liquidity_index numeric          NOT NULL,
  borrow_index    numeric          NOT NULL,
  total_supply    numeric,
  total_borrow    numeric,
  utilization     double precision,
  borrow_rate     numeric,
  supply_rate     numeric,
  ltv             integer,
  liq_threshold   integer,
  liq_bonus       integer,
  supply_cap      numeric,
  borrow_cap      numeric,
  is_frozen       boolean,
  is_paused       boolean,
  PRIMARY KEY (protocol, chain_id, asset, block_number, ts)
);
SELECT create_hypertable('reserve_state', 'ts', if_not_exists => TRUE);

-- Current position per account and asset (scaled balances, as stored on-chain)
CREATE TABLE IF NOT EXISTS position (
  protocol           text    NOT NULL,
  chain_id           bigint  NOT NULL,
  account            text    NOT NULL,
  asset              text    NOT NULL,
  scaled_supply      numeric NOT NULL DEFAULT 0,
  scaled_debt        numeric NOT NULL DEFAULT 0,
  collateral_enabled boolean NOT NULL DEFAULT false,
  last_block         bigint  NOT NULL,
  PRIMARY KEY (protocol, chain_id, account, asset)
);
CREATE INDEX IF NOT EXISTS position_debt_idx ON position (protocol, chain_id) WHERE scaled_debt > 0;

-- Oracle updates (hypertable)
CREATE TABLE IF NOT EXISTS oracle_update (
  chain_id     bigint      NOT NULL,
  feed         text        NOT NULL,
  block_number bigint      NOT NULL,
  round_id     numeric     NOT NULL,
  answer       numeric     NOT NULL,
  updated_at   timestamptz NOT NULL,
  ts           timestamptz NOT NULL,
  PRIMARY KEY (chain_id, feed, block_number, round_id, ts)
);
SELECT create_hypertable('oracle_update', 'ts', if_not_exists => TRUE);

-- Liquidations (hypertable)
CREATE TABLE IF NOT EXISTS liquidation (
  protocol          text        NOT NULL,
  chain_id          bigint      NOT NULL,
  block_number      bigint      NOT NULL,
  tx_hash           text        NOT NULL,
  log_index         integer     NOT NULL,
  account           text        NOT NULL,
  collateral_asset  text        NOT NULL,
  debt_asset        text        NOT NULL,
  debt_repaid       numeric     NOT NULL,
  collateral_seized numeric     NOT NULL,
  liquidator        text        NOT NULL,
  gas_price_wei     numeric,
  ts                timestamptz NOT NULL,
  PRIMARY KEY (chain_id, tx_hash, log_index, ts)
);
SELECT create_hypertable('liquidation', 'ts', if_not_exists => TRUE);

-- Computed metrics (hypertable)
CREATE TABLE IF NOT EXISTS risk_snapshot (
  ts          timestamptz      NOT NULL,
  protocol    text             NOT NULL,
  chain_id    bigint           NOT NULL,
  metric      text             NOT NULL,
  params      jsonb            NOT NULL DEFAULT '{}'::jsonb,
  value       double precision NOT NULL,
  block_number bigint
);
SELECT create_hypertable('risk_snapshot', 'ts', if_not_exists => TRUE);
CREATE INDEX IF NOT EXISTS risk_snapshot_metric_idx ON risk_snapshot (protocol, metric, ts DESC);

-- Alert history
CREATE TABLE IF NOT EXISTS alert (
  id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
  ts           timestamptz NOT NULL DEFAULT now(),
  rule_id      text        NOT NULL,
  severity     text        NOT NULL,
  status       text        NOT NULL,       -- firing | resolved
  metric_value double precision,
  threshold    text,
  payload      jsonb       NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS alert_rule_ts_idx ON alert (rule_id, ts DESC);
