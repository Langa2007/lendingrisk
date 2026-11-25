//! The indexing engine: backfill, live tail, reorg handling. Generic over chain, store and emitter
//! so every behaviour (restart, idempotency, reorgs) is unit-tested with a scripted fake chain.
use crate::chain::ChainSource;
use crate::decode::{AaveV3Adapter, ProtocolAdapter};
use crate::emit::Emitter;
use crate::metrics;
use crate::store::Store;
use crate::types::*;
use alloy::primitives::Address;
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub protocol: String,
    pub chain_id: u64,
    /// Backfill stops this many blocks below head so it can never be reorged.
    pub confirmations: u64,
    pub chunk_size: u64,
    pub min_chunk: u64,
    pub poll: Duration,
    /// Deepest reorg we will repair automatically; deeper means stop and alert a human.
    pub reorg_depth: u64,
    /// If we are further behind than this, catch up with chunked backfill first.
    pub catchup_threshold: u64,
    pub reserve_refresh_blocks: u64,
    pub retain_blocks: u64,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            protocol: "aave_v3".into(),
            chain_id: 1,
            confirmations: 12,
            chunk_size: 1_000,
            min_chunk: 10,
            poll: Duration::from_secs(2),
            reorg_depth: 64,
            catchup_threshold: 200,
            reserve_refresh_blocks: 1_800,
            retain_blocks: 1_000,
        }
    }
}

enum Step {
    Done,
    Reorg { ancestor: u64 },
}

pub struct Indexer<C: ChainSource, S: Store, E: Emitter> {
    pub chain: C,
    pub store: S,
    pub emitter: E,
    pub cfg: IndexerConfig,
    adapter: Box<dyn ProtocolAdapter>,
    tokens: TokenMap,
    filter: LogFilter,
    last_reserve_refresh: u64,
    last_prune: u64,
}

impl<C: ChainSource, S: Store, E: Emitter> Indexer<C, S, E> {
    pub fn new(chain: C, store: S, emitter: E, cfg: IndexerConfig) -> Self {
        Self {
            chain,
            store,
            emitter,
            cfg,
            adapter: Box::new(AaveV3Adapter),
            tokens: TokenMap::default(),
            filter: LogFilter::default(),
            last_reserve_refresh: 0,
            last_prune: 0,
        }
    }

    /// Resolve pool + reserves from chain and build the log filter.
    pub async fn init(&mut self) -> Result<()> {
        self.refresh_reserves().await?;
        info!(reserves = self.tokens.len(), "indexer initialised");
        Ok(())
    }

    async fn refresh_reserves(&mut self) -> Result<()> {
        let info = self.chain.load_protocol().await?;
        let head = self.chain.head().await?;
        self.store.save_tokens(&info.reserves, head).await?;
        self.tokens = TokenMap::from_reserves(info.reserves);
        let mut addresses: Vec<Address> = vec![info.pool];
        addresses.extend(self.tokens.atokens());
        addresses.extend(info.oracle_feeds);
        addresses.sort();
        addresses.dedup();
        self.filter = LogFilter {
            addresses,
            topics0: self.adapter.topics0(),
        };
        self.last_reserve_refresh = head;
        Ok(())
    }

    /// Process ONE block's logs: decode -> read exact balances -> atomic write -> notify.
    pub async fn process_block(
        &mut self,
        header: &Header,
        logs: &[RawLog],
        record_block: bool,
    ) -> Result<usize> {
        let mut decoded = self.adapter.decode(logs, &self.tokens)?;
        if decoded
            .touches
            .iter()
            .any(|(asset, _)| !self.tokens.contains(asset))
        {
            warn!(
                block = header.number,
                "event for unknown reserve: refreshing reserve list"
            );
            self.refresh_reserves().await?;
            decoded = self.adapter.decode(logs, &self.tokens)?;
        }
        let keys: Vec<(Address, Address)> = decoded
            .touches
            .iter()
            .copied()
            .filter(|(a, _)| self.tokens.contains(a))
            .collect();
        if keys.len() != decoded.touches.len() {
            warn!(
                block = header.number,
                dropped = decoded.touches.len() - keys.len(),
                "touches for still-unknown reserves dropped"
            );
        }
        metrics::EVENTS_DECODED.add(keys.len() as u64);

        let positions = if keys.is_empty() {
            Vec::new()
        } else {
            self.chain
                .read_positions(header.hash, &keys, &self.tokens)
                .await?
        };
        let n = positions.len();

        let write = BlockWrite {
            header: header.clone(),
            record_block,
            positions,
            reserves: decoded.reserve_updates,
            liquidations: decoded.liquidations,
            oracle_updates: decoded.oracle_updates,
            emodes: decoded.emodes,
        };
        self.store.apply_block(&write).await?;

        metrics::POSITIONS_WRITTEN.add(n as u64);
        metrics::BLOCKS_PROCESSED.inc();
        if record_block {
            metrics::CURSOR_BLOCK.set(header.number);
            metrics::LAST_PROCESSED_UNIX.set(header.timestamp);
            self.emitter
                .block_processed(&self.cfg.protocol, self.cfg.chain_id, header, n)
                .await;
            for update in &write.oracle_updates {
                self.emitter
                    .oracle_update(&self.cfg.protocol, self.cfg.chain_id, header, update)
                    .await;
            }
        }
        Ok(n)
    }

    /// Chunked historical indexing of [from, to]. Caller must keep `to` <= head - confirmations.
    pub async fn backfill(&mut self, from: u64, to: u64) -> Result<()> {
        if self.tokens.is_empty() {
            self.init().await?;
        }
        let mut start = from;
        let mut size = self.cfg.chunk_size.max(1);
        while start <= to {
            let end = (start + size - 1).min(to);
            let logs = match self.chain.logs_in_range(start, end, &self.filter).await {
                Ok(l) => l,
                Err(e) if size > self.cfg.min_chunk => {
                    size = (size / 2).max(self.cfg.min_chunk);
                    warn!(error = %e, new_chunk = size, "eth_getLogs failed: shrinking range (provider limit?)");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut by_block: BTreeMap<u64, Vec<RawLog>> = BTreeMap::new();
            for l in logs {
                by_block.entry(l.block_number).or_default().push(l);
            }
            let event_blocks = by_block.len();
            for (bn, mut blogs) in by_block {
                blogs.sort_by_key(|l| l.log_index);
                let header = self.chain.header(bn).await?;
                self.process_block(&header, &blogs, false).await?;
            }
            let end_header = self.chain.header(end).await?;
            self.store.put_checkpoint(&end_header).await?;
            metrics::CURSOR_BLOCK.set(end);
            info!(from = start, to = end, event_blocks, "backfill chunk done");
            start = end + 1;
            if size < self.cfg.chunk_size {
                size = (size * 2).min(self.cfg.chunk_size);
            }
        }
        Ok(())
    }

    /// Establish where to begin when the store is empty.
    pub async fn seed(&mut self, start_block: u64) -> Result<()> {
        let head = self.chain.head().await?;
        let safe = head.saturating_sub(self.cfg.confirmations);
        if start_block > safe {
            let cp = self.chain.header(start_block.saturating_sub(1)).await?;
            self.store.put_checkpoint(&cp).await?;
            info!(
                block = cp.number,
                "start block is near head: live tail begins right after it"
            );
        } else {
            self.backfill(start_block, safe).await?;
        }
        Ok(())
    }

    /// One poll: catch up to head, repairing reorgs on the way. Returns blocks processed.
    pub async fn sync_once(&mut self) -> Result<u64> {
        if self.tokens.is_empty() {
            self.init().await?;
        }
        let head = self.chain.head().await?;
        metrics::HEAD_BLOCK.set(head);
        let (cursor, _) = self
            .store
            .latest_processed()
            .await?
            .ok_or_else(|| anyhow!("store is empty: run `indexer backfill --from <block>` or `indexer run --start-block <block>` first"))?;

        // Every poll: is the block we last processed still canonical? (Catches reorgs that leave the
        // chain no longer than our cursor, which the parent-hash check in `step` alone would miss.)
        self.verify_tip(head, cursor).await?;
        let (cursor, _) = self
            .store
            .latest_processed()
            .await?
            .ok_or_else(|| anyhow!("cursor vanished"))?;

        let safe = head.saturating_sub(self.cfg.confirmations);
        if safe > cursor && safe - cursor > self.cfg.catchup_threshold {
            info!(
                cursor,
                head, "far behind: catching up with chunked backfill"
            );
            self.backfill(cursor + 1, safe).await?;
        }

        let (cursor, _) = self
            .store
            .latest_processed()
            .await?
            .ok_or_else(|| anyhow!("cursor vanished"))?;
        let mut next = cursor + 1;
        let mut processed = 0u64;
        let mut reorgs_this_poll = 0;
        while next <= head {
            match self.step(next).await? {
                Step::Done => {
                    next += 1;
                    processed += 1;
                }
                Step::Reorg { ancestor } => {
                    reorgs_this_poll += 1;
                    if reorgs_this_poll > 10 {
                        bail!("chain keeps reorganising (>10 reorgs in one poll); pausing");
                    }
                    next = ancestor + 1;
                }
            }
        }

        if head.saturating_sub(self.last_reserve_refresh) >= self.cfg.reserve_refresh_blocks {
            self.refresh_reserves().await?;
        }
        if head / 1_000 != self.last_prune / 1_000 {
            self.store
                .prune_processed(head.saturating_sub(self.cfg.retain_blocks))
                .await?;
            self.last_prune = head;
        }
        Ok(processed)
    }

    /// Compare our stored hash with the canonical hash at min(cursor, head). A mismatch means our tip
    /// was reorged away: roll back to the common ancestor. If the node merely reports a LOWER head on
    /// the same chain (lagging provider), hashes match and we do nothing and wait.
    async fn verify_tip(&mut self, head: u64, cursor: u64) -> Result<()> {
        let k = cursor.min(head);
        let Some(stored) = self.store.processed_hash(k).await? else {
            return Ok(());
        };
        if self.chain.header(k).await?.hash == stored {
            return Ok(());
        }
        let ancestor = self.find_ancestor(k).await?;
        let depth = cursor - ancestor;
        metrics::REORGS.inc();
        if depth > metrics::MAX_REORG_DEPTH.get() {
            metrics::MAX_REORG_DEPTH.set(depth);
        }
        warn!(
            cursor,
            head, ancestor, depth, "REORG detected at our tip: rolling back to common ancestor"
        );
        self.store.rollback_to(ancestor).await?;
        metrics::CURSOR_BLOCK.set(ancestor);
        Ok(())
    }

    async fn step(&mut self, n: u64) -> Result<Step> {
        let header = self.chain.header(n).await?;
        if n > 0 {
            if let Some(stored_parent) = self.store.processed_hash(n - 1).await? {
                if stored_parent != header.parent_hash {
                    let ancestor = self.find_ancestor(n - 1).await?;
                    let depth = (n - 1) - ancestor;
                    metrics::REORGS.inc();
                    if depth > metrics::MAX_REORG_DEPTH.get() {
                        metrics::MAX_REORG_DEPTH.set(depth);
                    }
                    warn!(
                        at = n,
                        ancestor, depth, "REORG detected: rolling back to common ancestor"
                    );
                    self.store.rollback_to(ancestor).await?;
                    metrics::CURSOR_BLOCK.set(ancestor);
                    return Ok(Step::Reorg { ancestor });
                }
            }
        }
        let mut logs = self.chain.logs_at_block(header.hash, &self.filter).await?;
        logs.sort_by_key(|l| l.log_index);
        self.process_block(&header, &logs, true).await?;
        Ok(Step::Done)
    }

    /// Walk back from `k` until our stored hash equals the canonical hash.
    async fn find_ancestor(&self, mut k: u64) -> Result<u64> {
        let floor = k.saturating_sub(self.cfg.reorg_depth);
        loop {
            match self.store.processed_hash(k).await? {
                None => return Ok(k), // no record below here: nothing more we can verify
                Some(stored) => {
                    if self.chain.header(k).await?.hash == stored {
                        return Ok(k);
                    }
                }
            }
            if k <= floor {
                bail!(
                    "reorg deeper than REORG_DEPTH={} blocks: stop and investigate (RPC returning inconsistent data, or a real deep reorg). Re-seed from an earlier block if needed.",
                    self.cfg.reorg_depth
                );
            }
            k -= 1;
        }
    }

    /// Run forever until ctrl-c. Errors are logged and retried, never fatal (except a stopped store).
    pub async fn run_live(&mut self) -> Result<()> {
        info!("live tail started");
        let mut head_notifications = match self.chain.subscribe_new_heads().await {
            Ok(stream) => stream,
            Err(error) => {
                warn!(error = %error, "WebSocket subscription failed; falling back to polling");
                None
            }
        };
        let mut backoff = self.cfg.poll;
        loop {
            match self.sync_once().await {
                Ok(n) => {
                    backoff = self.cfg.poll;
                    if n > 0 {
                        info!(blocks = n, "synced");
                    }
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "sync failed; will retry");
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            if let Some(receiver) = head_notifications.as_mut() {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => { info!("ctrl-c: shutting down"); return Ok(()); }
                    _ = tokio::time::sleep(backoff) => {}
                    event = receiver.recv() => {
                        if event.is_none() {
                            head_notifications = None;
                            warn!("WebSocket subscription closed; falling back to polling");
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => { info!("ctrl-c: shutting down"); return Ok(()); }
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::*;
    use crate::decode::test_logs::*;
    use crate::testutil::*;
    use alloy::primitives::{address, B256, U256};

    const POOL: Address = address!("00000000000000000000000000000000000000aa");
    const DP: Address = address!("00000000000000000000000000000000000000dd");
    const WETH: Address = address!("00000000000000000000000000000000000000e1");
    const USDC: Address = address!("00000000000000000000000000000000000000c1");
    const AWETH: Address = address!("00000000000000000000000000000000000000a1");
    const AUSDC: Address = address!("00000000000000000000000000000000000000a2");
    const ALICE: Address = address!("0000000000000000000000000000000000000a11");
    const BOB: Address = address!("0000000000000000000000000000000000000b0b");

    fn reserves(with_usdc: bool) -> Vec<ReserveTokens> {
        let mut v = vec![ReserveTokens {
            asset: WETH,
            atoken: AWETH,
            variable_debt_token: Address::repeat_byte(0x71),
            symbol: "WETH".into(),
            decimals: 18,
        }];
        if with_usdc {
            v.push(ReserveTokens {
                asset: USDC,
                atoken: AUSDC,
                variable_debt_token: Address::repeat_byte(0x72),
                symbol: "USDC".into(),
                decimals: 6,
            });
        }
        v
    }
    fn info(with_usdc: bool) -> ProtocolInfo {
        ProtocolInfo {
            pool: POOL,
            data_provider: DP,
            reserves: reserves(with_usdc),
            oracle_feeds: vec![],
        }
    }
    fn cfg() -> IndexerConfig {
        IndexerConfig {
            chunk_size: 10,
            min_chunk: 2,
            catchup_threshold: 1_000,
            confirmations: 0,
            reorg_depth: 16,
            ..Default::default()
        }
    }
    fn setup() -> (
        FakeChain,
        MemStore,
        RecordingEmitter,
        Indexer<FakeChain, MemStore, RecordingEmitter>,
    ) {
        let chain = FakeChain::new(info(true));
        let store = MemStore::default();
        let em = RecordingEmitter::default();
        let idx = Indexer::new(chain.clone(), store.clone(), em.clone(), cfg());
        (chain, store, em, idx)
    }
    fn supply_log(who: Address, amount: u64) -> impl FnOnce(u64) -> Vec<RawLog> {
        move |n| vec![raw(&supply(WETH, who, who, amount), POOL, n, 0)]
    }
    fn borrow_log(who: Address, amount: u64) -> impl FnOnce(u64) -> Vec<RawLog> {
        move |n| vec![raw(&borrow(USDC, who, who, amount), POOL, n, 0)]
    }
    fn u(x: u64) -> U256 {
        U256::from(x)
    }

    #[tokio::test]
    async fn live_tracks_positions_reserves_and_notifies() {
        let (chain, store, em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        chain.push(
            1,
            supply_log(ALICE, 100),
            vec![((WETH, ALICE), (100, 0, true))],
        );
        chain.push(
            1,
            borrow_log(ALICE, 50),
            vec![((USDC, ALICE), (0, 50, false))],
        );
        chain.push(1, |_| vec![], vec![]);

        assert_eq!(idx.sync_once().await.unwrap(), 3);
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((1, u(100), u(0), u(0), true))
        );
        assert_eq!(
            store.position(USDC, ALICE).await.unwrap(),
            Some((2, u(0), u(50), u(0), false))
        );
        assert_eq!(store.position(WETH, BOB).await.unwrap(), None);
        assert_eq!(*em.blocks.lock().unwrap(), vec![1, 2, 3]);
        assert_eq!(store.latest_processed().await.unwrap().unwrap().0, 3);
        // nothing new: next poll does nothing
        assert_eq!(idx.sync_once().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn reserve_updates_and_liquidations_are_stored() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        chain.push(
            1,
            |n| {
                vec![
                    raw(
                        &ReserveDataUpdated {
                            reserve: WETH,
                            liquidityRate: u(1),
                            stableBorrowRate: u(0),
                            variableBorrowRate: u(2),
                            liquidityIndex: u(1_050),
                            variableBorrowIndex: u(1_100),
                        },
                        POOL,
                        n,
                        0,
                    ),
                    raw(
                        &LiquidationCall {
                            collateralAsset: WETH,
                            debtAsset: USDC,
                            user: BOB,
                            debtToCover: u(10),
                            liquidatedCollateralAmount: u(5),
                            liquidator: ALICE,
                            receiveAToken: false,
                        },
                        POOL,
                        n,
                        1,
                    ),
                ]
            },
            vec![((WETH, BOB), (0, 0, false)), ((USDC, BOB), (0, 0, false))],
        );
        idx.sync_once().await.unwrap();
        assert_eq!(store.reserve_index(WETH, 1), Some(u(1_050)));
        assert_eq!(store.liquidation_count(), 1);
    }

    #[tokio::test]
    async fn restart_resumes_and_reapplying_a_block_is_idempotent() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        chain.push(
            1,
            supply_log(ALICE, 10),
            vec![((WETH, ALICE), (10, 0, true))],
        );
        let h2 = chain.push(
            1,
            supply_log(ALICE, 20),
            vec![((WETH, ALICE), (30, 0, true))],
        );
        assert_eq!(idx.sync_once().await.unwrap(), 2);
        drop(idx);

        // "crash and restart": a fresh indexer on the same store resumes after block 2
        let mut idx2 = Indexer::new(
            chain.clone(),
            store.clone(),
            RecordingEmitter::default(),
            cfg(),
        );
        chain.push(1, supply_log(BOB, 5), vec![((WETH, BOB), (5, 0, true))]);
        assert_eq!(idx2.sync_once().await.unwrap(), 1);

        // re-apply block 2 as if the process died after the write but before moving on
        let logs = chain
            .logs_at_block(h2.hash, &LogFilter::default())
            .await
            .unwrap();
        idx2.process_block(&h2, &logs, true).await.unwrap();
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((2, u(30), u(0), u(0), true))
        );
        assert_eq!(
            store.position(WETH, BOB).await.unwrap(),
            Some((3, u(5), u(0), u(0), true))
        );
    }

    #[tokio::test]
    async fn reorg_rolls_back_restores_previous_state_and_replays_new_fork() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        chain.push(
            1,
            supply_log(ALICE, 100),
            vec![((WETH, ALICE), (100, 0, true))],
        ); // 1
        chain.push(1, supply_log(BOB, 10), vec![((WETH, BOB), (10, 0, true))]); // 2
        chain.push(
            1,
            |n| {
                vec![
                    raw(&supply(WETH, BOB, BOB, 60), POOL, n, 0),
                    raw(
                        &ReserveDataUpdated {
                            reserve: WETH,
                            liquidityRate: u(1),
                            stableBorrowRate: u(0),
                            variableBorrowRate: u(1),
                            liquidityIndex: u(7),
                            variableBorrowIndex: u(7),
                        },
                        POOL,
                        n,
                        1,
                    ),
                ]
            },
            vec![((WETH, BOB), (70, 0, true))],
        ); // 3
        chain.push(
            1,
            borrow_log(ALICE, 5),
            vec![((USDC, ALICE), (0, 5, false))],
        ); // 4
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.position(WETH, BOB).await.unwrap(),
            Some((3, u(70), u(0), u(0), true))
        );
        assert_eq!(
            store.position(USDC, ALICE).await.unwrap(),
            Some((4, u(0), u(5), u(0), false))
        );
        assert_eq!(store.reserve_index(WETH, 3), Some(u(7)));

        // Fork B replaces blocks 3 and 4, and extends to 5
        chain.truncate_to(2);
        chain.push(
            2,
            supply_log(ALICE, 50),
            vec![((WETH, ALICE), (150, 0, true))],
        ); // 3'
        chain.push(2, |_| vec![], vec![]); // 4'
        chain.push(2, |_| vec![], vec![]); // 5'
        idx.sync_once().await.unwrap();

        assert_eq!(
            store.position(WETH, BOB).await.unwrap(),
            Some((2, u(10), u(0), u(0), true)),
            "BOB restored to pre-fork value"
        );
        assert_eq!(
            store.position(USDC, ALICE).await.unwrap(),
            None,
            "debt that only existed on the dead fork is gone"
        );
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((3, u(150), u(0), u(0), true)),
            "new fork applied"
        );
        assert_eq!(
            store.reserve_index(WETH, 3),
            None,
            "reserve row from the dead block removed"
        );
        let (n, h) = store.latest_processed().await.unwrap().unwrap();
        assert_eq!((n, h), (5, chain.header_sync(5).hash));
    }

    #[tokio::test]
    async fn reorg_to_a_shorter_chain_is_detected_at_the_tip() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        chain.push(1, supply_log(ALICE, 1), vec![((WETH, ALICE), (1, 0, true))]); // 1
        for _ in 0..4 {
            chain.push(1, |_| vec![], vec![]); // 2..5
        }
        chain.push(
            1,
            supply_log(ALICE, 9),
            vec![((WETH, ALICE), (10, 0, true))],
        ); // 6
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((6, u(10), u(0), u(0), true))
        );

        // canonical chain is now SHORTER than our cursor (head 5 < cursor 6) and block 5 differs
        chain.truncate_to(4);
        chain.push(2, |_| vec![], vec![]); // 5'
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.latest_processed().await.unwrap().unwrap(),
            (5, chain.header_sync(5).hash)
        );
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((1, u(1), u(0), u(0), true)),
            "block-6 data rolled back"
        );
    }

    #[tokio::test]
    async fn lagging_node_on_the_same_chain_does_not_trigger_a_rollback() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        for _ in 0..3 {
            chain.push(1, supply_log(ALICE, 1), vec![((WETH, ALICE), (1, 0, true))]);
        }
        idx.sync_once().await.unwrap();
        let before = store.position(WETH, ALICE).await.unwrap();
        chain.truncate_to(2); // provider failover to a node that is one block behind, same chain
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.latest_processed().await.unwrap().unwrap().0,
            3,
            "nothing rolled back"
        );
        assert_eq!(store.position(WETH, ALICE).await.unwrap(), before);
    }

    #[tokio::test]
    async fn reorg_deeper_than_limit_stops_instead_of_guessing() {
        let (chain, store, _em, mut idx) = setup();
        idx.cfg.reorg_depth = 2;
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        for _ in 0..6 {
            chain.push(1, |_| vec![], vec![]);
        }
        idx.sync_once().await.unwrap();
        chain.truncate_to(2);
        for _ in 0..5 {
            chain.push(2, |_| vec![], vec![]);
        }
        let err = idx.sync_once().await.unwrap_err().to_string();
        assert!(err.contains("deeper"), "{err}");
    }

    #[tokio::test]
    async fn backfill_uses_chunks_shrinks_on_provider_limits_and_only_checkpoints_the_cursor() {
        let (chain, store, _em, mut idx) = setup();
        chain.push(1, |_| vec![], vec![]); // 1
        for n in 2..=25u64 {
            match n {
                5 => chain.push(1, supply_log(ALICE, 1), vec![((WETH, ALICE), (1, 0, true))]),
                17 => chain.push(1, supply_log(ALICE, 2), vec![((WETH, ALICE), (3, 0, true))]),
                21 => chain.push(1, borrow_log(BOB, 9), vec![((USDC, BOB), (0, 9, false))]),
                _ => chain.push(1, |_| vec![], vec![]),
            };
        }
        chain.limit_range(4); // provider rejects ranges > 4 blocks: chunk 10 must shrink
        idx.backfill(1, 25).await.unwrap();

        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((17, u(3), u(0), u(0), true))
        );
        assert_eq!(
            store.position(USDC, BOB).await.unwrap(),
            Some((21, u(0), u(9), u(0), false))
        );
        // event blocks are NOT recorded as cursor positions during backfill, only chunk-end checkpoints are
        let rec = store.processed_blocks();
        assert!(
            !rec.contains(&5) && !rec.contains(&17) && !rec.contains(&21),
            "{rec:?}"
        );
        assert_eq!(
            store.latest_processed().await.unwrap().unwrap(),
            (25, chain.header_sync(25).hash)
        );
    }

    #[tokio::test]
    async fn crash_mid_backfill_cannot_skip_part_of_a_chunk() {
        let (chain, store, _em, mut idx) = setup();
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        for n in 1..=10u64 {
            if n == 3 || n == 8 {
                chain.push(1, supply_log(ALICE, n), vec![((WETH, ALICE), (n, 0, true))]);
            } else {
                chain.push(1, |_| vec![], vec![]);
            }
        }
        // simulate: block 3 processed, then the process died before the chunk checkpoint
        let h3 = chain.header_sync(3);
        let logs3 = chain
            .logs_at_block(h3.hash, &LogFilter::default())
            .await
            .unwrap();
        idx.init().await.unwrap();
        idx.process_block(&h3, &logs3, false).await.unwrap();
        // the cursor must still be at genesis, so a restart redoes blocks 1..10
        assert_eq!(store.latest_processed().await.unwrap().unwrap().0, 0);
        idx.backfill(1, 10).await.unwrap();
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((8, u(8), u(0), u(0), true))
        );
    }

    #[tokio::test]
    async fn unknown_reserve_triggers_a_refresh_instead_of_losing_the_event() {
        let chain = FakeChain::new(info(false)); // USDC not listed yet
        let store = MemStore::default();
        let mut idx = Indexer::new(
            chain.clone(),
            store.clone(),
            RecordingEmitter::default(),
            cfg(),
        );
        store.put_checkpoint(&chain.genesis()).await.unwrap();
        idx.init().await.unwrap();
        chain.set_info(info(true)); // governance lists USDC
        chain.push(
            1,
            borrow_log(ALICE, 7),
            vec![((USDC, ALICE), (0, 7, false))],
        );
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.position(USDC, ALICE).await.unwrap(),
            Some((1, u(0), u(7), u(0), false))
        );
    }

    #[tokio::test]
    async fn seed_near_head_starts_live_right_after_the_start_block() {
        let (chain, store, _em, mut idx) = setup();
        idx.cfg.confirmations = 12;
        for n in 1..=30u64 {
            if n == 26 {
                chain.push(1, supply_log(ALICE, 4), vec![((WETH, ALICE), (4, 0, true))]);
            } else if n == 10 {
                chain.push(1, supply_log(BOB, 9), vec![((WETH, BOB), (9, 0, true))]);
            } else {
                chain.push(1, |_| vec![], vec![]);
            }
        }
        idx.seed(25).await.unwrap(); // 25 > head(30) - 12 = 18
        assert_eq!(store.latest_processed().await.unwrap().unwrap().0, 24);
        idx.sync_once().await.unwrap();
        assert_eq!(
            store.position(WETH, ALICE).await.unwrap(),
            Some((26, u(4), u(0), u(0), true))
        );
        assert_eq!(
            store.position(WETH, BOB).await.unwrap(),
            None,
            "before the start block: out of scope"
        );
    }

    #[tokio::test]
    async fn empty_store_gives_an_actionable_error() {
        let (_chain, _store, _em, mut idx) = setup();
        let e = idx.sync_once().await.unwrap_err().to_string();
        assert!(e.contains("backfill") || e.contains("start-block"), "{e}");
    }

    // ---- store contract: same scenario for MemStore always, PgStore when TEST_DATABASE_URL is set ----
    pub async fn store_scenario<S: Store>(s: &S) {
        let hdr = |n: u64, salt: u8| Header {
            number: n,
            hash: B256::repeat_byte(salt),
            parent_hash: B256::repeat_byte(salt.wrapping_sub(1)),
            timestamp: 1_700_000_000 + n,
        };
        let pr = |sup: u64, debt: u64, stable_debt: u64| PositionRead {
            asset: WETH,
            account: ALICE,
            scaled_supply: u(sup),
            scaled_debt: u(debt),
            stable_debt: u(stable_debt),
            collateral_enabled: true,
        };
        let w = |n: u64, p: Vec<PositionRead>| BlockWrite {
            header: hdr(n, n as u8 + 10),
            record_block: true,
            positions: p,
            reserves: vec![],
            liquidations: vec![],
            oracle_updates: vec![OracleUpdate {
                feed: Address::repeat_byte(0x55),
                round_id: U256::from(n),
                answer: n.to_string(),
                updated_at: 1_700_000_000 + n,
                log_index: 0,
            }],
            emodes: vec![(BOB, n as u8)],
        };

        s.apply_block(&w(1, vec![pr(10, 0, 0)])).await.unwrap();
        s.apply_block(&w(2, vec![pr(20, 1, 7)])).await.unwrap();
        s.apply_block(&w(3, vec![pr(30, 2, 9)])).await.unwrap();
        s.apply_block(&w(3, vec![pr(30, 2, 9)])).await.unwrap(); // idempotent
        assert_eq!(
            s.position(WETH, ALICE).await.unwrap(),
            Some((3, u(30), u(2), u(9), true))
        );
        // an older block can never overwrite newer latest-state
        s.apply_block(&BlockWrite {
            record_block: false,
            ..w(2, vec![pr(999, 9, 10)])
        })
        .await
        .unwrap();
        assert_eq!(
            s.position(WETH, ALICE).await.unwrap(),
            Some((3, u(30), u(2), u(9), true))
        );
        s.apply_block(&w(2, vec![pr(20, 1, 7)])).await.unwrap();

        s.rollback_to(2).await.unwrap();
        assert_eq!(
            s.position(WETH, ALICE).await.unwrap(),
            Some((2, u(20), u(1), u(7), true))
        );
        assert_eq!(s.latest_processed().await.unwrap().unwrap().0, 2);
        assert!(s.processed_hash(3).await.unwrap().is_none());

        s.rollback_to(0).await.unwrap();
        assert_eq!(s.position(WETH, ALICE).await.unwrap(), None);
        assert!(s.latest_processed().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mem_store_contract() {
        store_scenario(&MemStore::default()).await;
    }

    #[tokio::test]
    async fn mem_store_oracle_updates_are_idempotent_and_reorg_safe() {
        let store = MemStore::default();
        let block = BlockWrite {
            header: Header {
                number: 1,
                hash: B256::repeat_byte(1),
                parent_hash: B256::ZERO,
                timestamp: 1_700_000_001,
            },
            record_block: true,
            positions: vec![],
            reserves: vec![],
            liquidations: vec![],
            oracle_updates: vec![OracleUpdate {
                feed: Address::repeat_byte(0x55),
                round_id: U256::from(1u64),
                answer: "123".into(),
                updated_at: 1_700_000_000,
                log_index: 3,
            }],
            emodes: vec![],
        };
        store.apply_block(&block).await.unwrap();
        store.apply_block(&block).await.unwrap();
        assert_eq!(store.oracle_update_count(), 1);
        store.rollback_to(0).await.unwrap();
        assert_eq!(store.oracle_update_count(), 0);
    }

    #[tokio::test]
    async fn pg_store_contract_when_database_available() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL not set: skipping PgStore contract test");
            return;
        };
        // unique chain id so parallel/old runs never collide
        let chain_id = 900_000_000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
                % 99_999_999;
        let pg = crate::store::PgStore::connect(&url, "aave_v3", chain_id)
            .await
            .unwrap();
        store_scenario(&pg).await;
    }

    #[tokio::test]
    async fn pg_store_reserve_liquidation_emode_tables_and_rollback() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL not set: skipping");
            return;
        };
        let chain_id = 910_000_000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
                % 89_999_999;
        let pg = crate::store::PgStore::connect(&url, "aave_v3", chain_id)
            .await
            .unwrap();
        let cid = chain_id as i64;
        let count = |table: &'static str| {
            let pool = pg.pool().clone();
            async move {
                let q = format!("SELECT count(*) FROM {table} WHERE chain_id = $1");
                sqlx::query_scalar::<_, i64>(&q)
                    .bind(cid)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let hdr = |n: u64| Header {
            number: n,
            hash: B256::repeat_byte(n as u8 + 1),
            parent_hash: B256::repeat_byte(n as u8),
            timestamp: 1_700_000_000 + n,
        };
        let block = |n: u64| BlockWrite {
            header: hdr(n),
            record_block: true,
            positions: vec![],
            reserves: vec![ReserveUpdate {
                asset: WETH,
                liquidity_rate: u(1),
                variable_borrow_rate: u(2),
                liquidity_index: u(1_000 + n),
                variable_borrow_index: u(2_000 + n),
                log_index: 0,
            }],
            liquidations: vec![LiquidationRow {
                collateral_asset: WETH,
                debt_asset: USDC,
                account: ALICE,
                debt_repaid: u(5),
                collateral_seized: u(6),
                liquidator: BOB,
                tx_hash: B256::repeat_byte(n as u8),
                log_index: 3,
            }],
            oracle_updates: vec![OracleUpdate {
                feed: Address::repeat_byte(0x55),
                round_id: U256::from(n),
                answer: n.to_string(),
                updated_at: 1_700_000_000 + n,
                log_index: 3,
            }],
            emodes: vec![(ALICE, n as u8)],
        };
        for n in 1..=3 {
            pg.apply_block(&block(n)).await.unwrap();
        }
        pg.apply_block(&block(3)).await.unwrap(); // idempotent
        assert_eq!(count("reserve_state").await, 3);
        assert_eq!(count("liquidation").await, 3);
        assert_eq!(count("account_emode_history").await, 3);
        assert_eq!(count("oracle_update").await, 3);
        let cat: i32 = sqlx::query_scalar("SELECT category FROM account_emode WHERE chain_id = $1")
            .bind(cid)
            .fetch_one(pg.pool())
            .await
            .unwrap();
        assert_eq!(cat, 3);

        pg.rollback_to(1).await.unwrap();
        assert_eq!(count("reserve_state").await, 1);
        assert_eq!(count("liquidation").await, 1);
        assert_eq!(count("oracle_update").await, 1);
        let cat: i32 = sqlx::query_scalar("SELECT category FROM account_emode WHERE chain_id = $1")
            .bind(cid)
            .fetch_one(pg.pool())
            .await
            .unwrap();
        assert_eq!(cat, 1, "emode restored to pre-reorg value");

        pg.rollback_to(0).await.unwrap();
        assert_eq!(count("reserve_state").await, 0);
        assert_eq!(count("account_emode").await, 0);
        assert_eq!(count("oracle_update").await, 0);
        pg.save_tokens(&reserves(true), 5).await.unwrap();
        assert_eq!(pg.load_tokens().await.unwrap().len(), 2);
    }
}
