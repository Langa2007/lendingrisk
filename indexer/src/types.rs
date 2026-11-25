//! Plain data types shared by every module. No I/O in here.
use alloy::primitives::{Address, Bytes, B256, U256};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
}

/// A log reduced to what the indexer needs (decoupled from any RPC library type).
#[derive(Debug, Clone)]
pub struct RawLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
    pub block_number: u64,
    pub tx_hash: B256,
    pub log_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveTokens {
    pub asset: Address,
    pub atoken: Address,
    pub variable_debt_token: Address,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, Default)]
pub struct TokenMap {
    by_asset: HashMap<Address, ReserveTokens>,
    by_atoken: HashMap<Address, Address>,
}

impl TokenMap {
    pub fn from_reserves(reserves: Vec<ReserveTokens>) -> Self {
        let mut m = TokenMap::default();
        for r in reserves {
            m.by_atoken.insert(r.atoken, r.asset);
            m.by_asset.insert(r.asset, r);
        }
        m
    }
    pub fn get(&self, asset: &Address) -> Option<&ReserveTokens> {
        self.by_asset.get(asset)
    }
    pub fn contains(&self, asset: &Address) -> bool {
        self.by_asset.contains_key(asset)
    }
    pub fn asset_for_atoken(&self, atoken: &Address) -> Option<Address> {
        self.by_atoken.get(atoken).copied()
    }
    pub fn atokens(&self) -> Vec<Address> {
        let mut v: Vec<Address> = self.by_atoken.keys().copied().collect();
        v.sort();
        v
    }
    pub fn len(&self) -> usize {
        self.by_asset.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_asset.is_empty()
    }
}

/// Exact on-chain scaled balances for one (asset, account) at one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRead {
    pub asset: Address,
    pub account: Address,
    pub scaled_supply: U256,
    /// Scaled variable debt, as returned by Aave's PoolDataProvider.
    pub scaled_debt: U256,
    /// Current stable debt; Aave exposes this as an accrued balance, not a scaled value.
    pub stable_debt: U256,
    pub collateral_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveUpdate {
    pub asset: Address,
    pub liquidity_rate: U256,
    pub variable_borrow_rate: U256,
    pub liquidity_index: U256,
    pub variable_borrow_index: U256,
    pub log_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationRow {
    pub collateral_asset: Address,
    pub debt_asset: Address,
    pub account: Address,
    pub debt_repaid: U256,
    pub collateral_seized: U256,
    pub liquidator: Address,
    pub tx_hash: B256,
    pub log_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleUpdate {
    pub feed: Address,
    pub round_id: U256,
    pub answer: String,
    pub updated_at: u64,
    pub log_index: u64,
}

/// Everything written for one block, in one atomic database transaction.
#[derive(Debug, Clone)]
pub struct BlockWrite {
    pub header: Header,
    /// true in live mode: record this block in processed_block (advances the cursor).
    /// false during backfill: the cursor only advances at chunk checkpoints, so a crash
    /// mid-chunk re-processes the whole chunk instead of skipping part of it.
    pub record_block: bool,
    pub positions: Vec<PositionRead>,
    pub reserves: Vec<ReserveUpdate>,
    pub liquidations: Vec<LiquidationRow>,
    pub oracle_updates: Vec<OracleUpdate>,
    pub emodes: Vec<(Address, u8)>,
}

#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub addresses: Vec<Address>,
    pub topics0: Vec<B256>,
}

#[derive(Debug, Clone)]
pub struct ProtocolInfo {
    pub pool: Address,
    #[allow(dead_code)] // resolved inside RpcChain for reads; kept for diagnostics
    pub data_provider: Address,
    pub reserves: Vec<ReserveTokens>,
    pub oracle_feeds: Vec<Address>,
}

pub fn addr_str(a: &Address) -> String {
    format!("{a:#x}")
}

pub fn hash_str(h: &B256) -> String {
    format!("{h:#x}")
}
