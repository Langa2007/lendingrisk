//! The blockchain as the indexer sees it. A trait, so tests can use a scripted fake chain
//! (including reorgs) and production uses the JSON-RPC implementation in rpc.rs.
use crate::types::*;
use alloy::primitives::{Address, B256};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ChainSource: Send + Sync {
    async fn head(&self) -> Result<u64>;
    /// Header of the CURRENT canonical block at `number`.
    async fn header(&self, number: u64) -> Result<Header>;
    async fn logs_in_range(&self, from: u64, to: u64, filter: &LogFilter) -> Result<Vec<RawLog>>;
    /// Logs of one specific block, addressed by hash (immune to reorg races).
    async fn logs_at_block(&self, hash: B256, filter: &LogFilter) -> Result<Vec<RawLog>>;
    /// Exact scaled balances + collateral flag for each (asset, account) as of the block `hash`.
    async fn read_positions(
        &self,
        hash: B256,
        keys: &[(Address, Address)],
        tokens: &TokenMap,
    ) -> Result<Vec<PositionRead>>;
    /// Resolve pool, data provider and the reserve -> token map (at the latest block).
    async fn load_protocol(&self) -> Result<ProtocolInfo>;
    /// Optional WebSocket notification stream for new heads; polling remains the fallback.
    async fn subscribe_new_heads(
        &self,
    ) -> Result<Option<tokio::sync::mpsc::UnboundedReceiver<()>>> {
        Ok(None)
    }
}
