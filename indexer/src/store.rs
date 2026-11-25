//! Persistence. `Store` is a trait so the core logic can be tested without a database;
//! `PgStore` is the real implementation (PostgreSQL / TimescaleDB).
use crate::types::*;
use alloy::primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::str::FromStr;

#[async_trait]
pub trait Store: Send + Sync {
    /// Highest processed block (number, hash), if any.
    async fn latest_processed(&self) -> Result<Option<(u64, B256)>>;
    async fn processed_hash(&self, number: u64) -> Result<Option<B256>>;
    /// Atomically persist everything for one block. MUST be idempotent (safe to repeat).
    async fn apply_block(&self, w: &BlockWrite) -> Result<()>;
    /// Record that blocks up to `h` are processed without per-block data (backfill chunk end).
    async fn put_checkpoint(&self, h: &Header) -> Result<()>;
    /// Delete all data for blocks > `number` and restore latest-state tables.
    async fn rollback_to(&self, number: u64) -> Result<()>;
    async fn save_tokens(&self, reserves: &[ReserveTokens], at_block: u64) -> Result<()>;
    #[allow(dead_code)] // used by tests and by the Phase 2 engine tooling
    async fn load_tokens(&self) -> Result<Vec<ReserveTokens>>;
    /// Forget processed_block rows older than `below` (keeps the table small).
    async fn prune_processed(&self, below: u64) -> Result<()>;
    /// Latest stored position: (last_block, scaled_supply, scaled_variable_debt, stable_debt, collateral_enabled).
    #[allow(dead_code)] // used by tests; handy for ad-hoc tooling
    async fn position(
        &self,
        asset: Address,
        account: Address,
    ) -> Result<Option<(u64, U256, U256, U256, bool)>>;
}

pub struct PgStore {
    pool: PgPool,
    protocol: String,
    chain_id: i64,
}

impl PgStore {
    pub async fn connect(url: &str, protocol: &str, chain_id: u64) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .context("connecting to postgres (is `make up` running and DATABASE_URL correct?)")?;
        Ok(Self {
            pool,
            protocol: protocol.to_string(),
            chain_id: chain_id as i64,
        })
    }

    #[allow(dead_code)] // used by tests
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn parse_hash(s: &str) -> Result<B256> {
    B256::from_str(s).with_context(|| format!("bad hash in database: {s}"))
}

#[async_trait]
impl Store for PgStore {
    async fn latest_processed(&self) -> Result<Option<(u64, B256)>> {
        let row = sqlx::query(
            "SELECT block_number, block_hash FROM processed_block WHERE chain_id = $1 ORDER BY block_number DESC LIMIT 1",
        )
        .bind(self.chain_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => {
                let n: i64 = r.get(0);
                let h: String = r.get(1);
                Ok(Some((n as u64, parse_hash(&h)?)))
            }
        }
    }

    async fn processed_hash(&self, number: u64) -> Result<Option<B256>> {
        let row = sqlx::query(
            "SELECT block_hash FROM processed_block WHERE chain_id = $1 AND block_number = $2",
        )
        .bind(self.chain_id)
        .bind(number as i64)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(parse_hash(&r.get::<String, _>(0))?)),
        }
    }

    async fn apply_block(&self, w: &BlockWrite) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let block = w.header.number as i64;

        if w.record_block {
            sqlx::query(
                "INSERT INTO processed_block (chain_id, block_number, block_hash, parent_hash, ts)
                 VALUES ($1, $2, $3, $4, to_timestamp($5::bigint))
                 ON CONFLICT (chain_id, block_number) DO UPDATE
                   SET block_hash = EXCLUDED.block_hash, parent_hash = EXCLUDED.parent_hash, ts = EXCLUDED.ts",
            )
            .bind(self.chain_id)
            .bind(block)
            .bind(hash_str(&w.header.hash))
            .bind(hash_str(&w.header.parent_hash))
            .bind(w.header.timestamp as i64)
            .execute(&mut *tx)
            .await?;
        }

        if !w.positions.is_empty() {
            let accounts: Vec<String> = w.positions.iter().map(|p| addr_str(&p.account)).collect();
            let assets: Vec<String> = w.positions.iter().map(|p| addr_str(&p.asset)).collect();
            let supply: Vec<String> = w
                .positions
                .iter()
                .map(|p| p.scaled_supply.to_string())
                .collect();
            let debt: Vec<String> = w
                .positions
                .iter()
                .map(|p| p.scaled_debt.to_string())
                .collect();
            let stable_debt: Vec<String> = w
                .positions
                .iter()
                .map(|p| p.stable_debt.to_string())
                .collect();
            let coll: Vec<bool> = w.positions.iter().map(|p| p.collateral_enabled).collect();

            sqlx::query(
                "INSERT INTO position_history (protocol, chain_id, account, asset, block_number, scaled_supply, scaled_debt, stable_debt, collateral_enabled)
                 SELECT $1::text, $2::bigint, t.a, t.s, $3::bigint, t.ss::numeric, t.sd::numeric, t.st::numeric, t.c
                 FROM UNNEST($4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::bool[]) AS t(a, s, ss, sd, st, c)
                 ON CONFLICT (protocol, chain_id, account, asset, block_number) DO UPDATE
                   SET scaled_supply = EXCLUDED.scaled_supply, scaled_debt = EXCLUDED.scaled_debt,
                       stable_debt = EXCLUDED.stable_debt,
                       collateral_enabled = EXCLUDED.collateral_enabled",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(block)
            .bind(&accounts)
            .bind(&assets)
            .bind(&supply)
            .bind(&debt)
            .bind(&stable_debt)
            .bind(&coll)
            .execute(&mut *tx)
            .await?;

            // Latest-state table: never let an older block overwrite a newer one.
            sqlx::query(
                "INSERT INTO position (protocol, chain_id, account, asset, scaled_supply, scaled_debt, stable_debt, collateral_enabled, last_block)
                 SELECT $1::text, $2::bigint, t.a, t.s, t.ss::numeric, t.sd::numeric, t.st::numeric, t.c, $3::bigint
                 FROM UNNEST($4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::bool[]) AS t(a, s, ss, sd, st, c)
                 ON CONFLICT (protocol, chain_id, account, asset) DO UPDATE
                   SET scaled_supply = EXCLUDED.scaled_supply, scaled_debt = EXCLUDED.scaled_debt,
                       stable_debt = EXCLUDED.stable_debt,
                       collateral_enabled = EXCLUDED.collateral_enabled, last_block = EXCLUDED.last_block
                   WHERE position.last_block <= EXCLUDED.last_block",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(block)
            .bind(&accounts)
            .bind(&assets)
            .bind(&supply)
            .bind(&debt)
            .bind(&stable_debt)
            .bind(&coll)
            .execute(&mut *tx)
            .await?;
        }

        for r in &w.reserves {
            sqlx::query(
                "INSERT INTO reserve_state (protocol, chain_id, asset, block_number, block_hash, ts,
                                            liquidity_index, borrow_index, borrow_rate, supply_rate)
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6::bigint), $7::numeric, $8::numeric, $9::numeric, $10::numeric)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(addr_str(&r.asset))
            .bind(block)
            .bind(hash_str(&w.header.hash))
            .bind(w.header.timestamp as i64)
            .bind(r.liquidity_index.to_string())
            .bind(r.variable_borrow_index.to_string())
            .bind(r.variable_borrow_rate.to_string())
            .bind(r.liquidity_rate.to_string())
            .execute(&mut *tx)
            .await?;
        }

        for l in &w.liquidations {
            sqlx::query(
                "INSERT INTO liquidation (protocol, chain_id, block_number, tx_hash, log_index, account, collateral_asset,
                                          debt_asset, debt_repaid, collateral_seized, liquidator, ts)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, $10::numeric, $11, to_timestamp($12::bigint))
                 ON CONFLICT DO NOTHING",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(block)
            .bind(hash_str(&l.tx_hash))
            .bind(l.log_index as i32)
            .bind(addr_str(&l.account))
            .bind(addr_str(&l.collateral_asset))
            .bind(addr_str(&l.debt_asset))
            .bind(l.debt_repaid.to_string())
            .bind(l.collateral_seized.to_string())
            .bind(addr_str(&l.liquidator))
            .bind(w.header.timestamp as i64)
            .execute(&mut *tx)
            .await?;
        }

        for update in &w.oracle_updates {
            sqlx::query(
                "INSERT INTO oracle_update (chain_id, feed, block_number, round_id, answer, updated_at, ts)
                 VALUES ($1, $2, $3, $4::numeric, $5::numeric,
                         to_timestamp($6::bigint), to_timestamp($7::bigint))
                 ON CONFLICT DO NOTHING",
            )
            .bind(self.chain_id)
            .bind(addr_str(&update.feed))
            .bind(block)
            .bind(update.round_id.to_string())
            .bind(&update.answer)
            .bind(update.updated_at as i64)
            .bind(w.header.timestamp as i64)
            .execute(&mut *tx)
            .await?;
        }

        for (account, category) in &w.emodes {
            sqlx::query(
                "INSERT INTO account_emode_history (protocol, chain_id, account, block_number, category)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (protocol, chain_id, account, block_number) DO UPDATE SET category = EXCLUDED.category",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(addr_str(account))
            .bind(block)
            .bind(*category as i32)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO account_emode (protocol, chain_id, account, category, last_block)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (protocol, chain_id, account) DO UPDATE
                   SET category = EXCLUDED.category, last_block = EXCLUDED.last_block
                   WHERE account_emode.last_block <= EXCLUDED.last_block",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(addr_str(account))
            .bind(*category as i32)
            .bind(block)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn put_checkpoint(&self, h: &Header) -> Result<()> {
        sqlx::query(
            "INSERT INTO processed_block (chain_id, block_number, block_hash, parent_hash, ts)
             VALUES ($1, $2, $3, $4, to_timestamp($5::bigint))
             ON CONFLICT (chain_id, block_number) DO UPDATE
               SET block_hash = EXCLUDED.block_hash, parent_hash = EXCLUDED.parent_hash, ts = EXCLUDED.ts",
        )
        .bind(self.chain_id)
        .bind(h.number as i64)
        .bind(hash_str(&h.hash))
        .bind(hash_str(&h.parent_hash))
        .bind(h.timestamp as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn rollback_to(&self, number: u64) -> Result<()> {
        let n = number as i64;
        let mut tx = self.pool.begin().await?;

        // 1. which (account, asset) keys were written above `n`?
        sqlx::query(
            "CREATE TEMP TABLE rb_pos ON COMMIT DROP AS
             SELECT DISTINCT account, asset FROM position_history
             WHERE protocol = $1 AND chain_id = $2 AND block_number > $3",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .bind(n)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "CREATE TEMP TABLE rb_em ON COMMIT DROP AS
             SELECT DISTINCT account FROM account_emode_history
             WHERE protocol = $1 AND chain_id = $2 AND block_number > $3",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .bind(n)
        .execute(&mut *tx)
        .await?;

        // 2. delete the rolled-back history
        for table in [
            "position_history",
            "account_emode_history",
            "reserve_state",
            "liquidation",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE protocol = $1 AND chain_id = $2 AND block_number > $3"
            ))
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(n)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM processed_block WHERE chain_id = $1 AND block_number > $2")
            .bind(self.chain_id)
            .bind(n)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM oracle_update WHERE chain_id = $1 AND block_number > $2")
            .bind(self.chain_id)
            .bind(n)
            .execute(&mut *tx)
            .await?;

        // 3. rebuild latest-state rows for the touched keys from what history remains
        sqlx::query(
            "DELETE FROM position p USING rb_pos k
             WHERE p.protocol = $1 AND p.chain_id = $2 AND p.account = k.account AND p.asset = k.asset",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO position (protocol, chain_id, account, asset, scaled_supply, scaled_debt, stable_debt, collateral_enabled, last_block)
             SELECT DISTINCT ON (h.account, h.asset)
                    h.protocol, h.chain_id, h.account, h.asset, h.scaled_supply, h.scaled_debt, h.stable_debt, h.collateral_enabled, h.block_number
             FROM position_history h JOIN rb_pos k ON k.account = h.account AND k.asset = h.asset
             WHERE h.protocol = $1 AND h.chain_id = $2
             ORDER BY h.account, h.asset, h.block_number DESC",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "DELETE FROM account_emode e USING rb_em k
             WHERE e.protocol = $1 AND e.chain_id = $2 AND e.account = k.account",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO account_emode (protocol, chain_id, account, category, last_block)
             SELECT DISTINCT ON (h.account) h.protocol, h.chain_id, h.account, h.category, h.block_number
             FROM account_emode_history h JOIN rb_em k ON k.account = h.account
             WHERE h.protocol = $1 AND h.chain_id = $2
             ORDER BY h.account, h.block_number DESC",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn save_tokens(&self, reserves: &[ReserveTokens], at_block: u64) -> Result<()> {
        for r in reserves {
            sqlx::query(
                "INSERT INTO reserve_token (protocol, chain_id, asset, atoken, variable_debt_token, symbol, decimals, updated_block)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (protocol, chain_id, asset) DO UPDATE
                   SET atoken = EXCLUDED.atoken, variable_debt_token = EXCLUDED.variable_debt_token,
                       symbol = EXCLUDED.symbol, decimals = EXCLUDED.decimals, updated_block = EXCLUDED.updated_block",
            )
            .bind(&self.protocol)
            .bind(self.chain_id)
            .bind(addr_str(&r.asset))
            .bind(addr_str(&r.atoken))
            .bind(addr_str(&r.variable_debt_token))
            .bind(&r.symbol)
            .bind(r.decimals as i32)
            .bind(at_block as i64)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn load_tokens(&self) -> Result<Vec<ReserveTokens>> {
        let rows = sqlx::query(
            "SELECT asset, atoken, variable_debt_token, symbol, decimals FROM reserve_token WHERE protocol = $1 AND chain_id = $2",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReserveTokens {
                    asset: Address::from_str(&r.get::<String, _>(0))?,
                    atoken: Address::from_str(&r.get::<String, _>(1))?,
                    variable_debt_token: Address::from_str(&r.get::<String, _>(2))?,
                    symbol: r.get(3),
                    decimals: r.get::<i32, _>(4) as u8,
                })
            })
            .collect()
    }

    async fn prune_processed(&self, below: u64) -> Result<()> {
        sqlx::query("DELETE FROM processed_block WHERE chain_id = $1 AND block_number < $2")
            .bind(self.chain_id)
            .bind(below as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn position(
        &self,
        asset: Address,
        account: Address,
    ) -> Result<Option<(u64, U256, U256, U256, bool)>> {
        let row = sqlx::query(
            "SELECT last_block, scaled_supply::text, scaled_debt::text, stable_debt::text, collateral_enabled
             FROM position WHERE protocol = $1 AND chain_id = $2 AND account = $3 AND asset = $4",
        )
        .bind(&self.protocol)
        .bind(self.chain_id)
        .bind(addr_str(&account))
        .bind(addr_str(&asset))
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some((
                r.get::<i64, _>(0) as u64,
                U256::from_str(&r.get::<String, _>(1))?,
                U256::from_str(&r.get::<String, _>(2))?,
                U256::from_str(&r.get::<String, _>(3))?,
                r.get(4),
            ))),
        }
    }
}
