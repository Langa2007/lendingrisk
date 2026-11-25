//! Test doubles: a scripted chain (with reorgs) and an in-memory store with the same
//! semantics as PgStore (history table + rollback rebuild). Only compiled for tests.
use crate::chain::ChainSource;
use crate::emit::Emitter;
use crate::store::Store;
use crate::types::*;
use alloy::primitives::{Address, B256, U256};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

type Delta = HashMap<(Address, Address), (U256, U256, bool)>;
type PositionHistoryRow = (U256, U256, U256, bool);
type LatestPosition = (u64, U256, U256, U256, bool);
/// Scripted balances for a block: ((asset, account), (scaled_supply, scaled_debt, collateral)).
pub type DeltaSpec = Vec<((Address, Address), (u64, u64, bool))>;

struct FakeBlock {
    header: Header,
    logs: Vec<RawLog>,
    deltas: Delta,
}

struct FakeInner {
    blocks: HashMap<B256, FakeBlock>,
    canonical: Vec<B256>,
    info: ProtocolInfo,
    max_range: Option<u64>,
}

#[derive(Clone)]
pub struct FakeChain {
    inner: Arc<Mutex<FakeInner>>,
}

fn block_hash(number: u64, salt: u8) -> B256 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&number.to_be_bytes());
    b[8] = salt;
    b[9] = 0xEE;
    B256::from(b)
}

impl FakeChain {
    pub fn new(info: ProtocolInfo) -> Self {
        let genesis = Header {
            number: 0,
            hash: block_hash(0, 0),
            parent_hash: B256::ZERO,
            timestamp: 1_700_000_000,
        };
        let mut blocks = HashMap::new();
        blocks.insert(
            genesis.hash,
            FakeBlock {
                header: genesis.clone(),
                logs: vec![],
                deltas: HashMap::new(),
            },
        );
        Self {
            inner: Arc::new(Mutex::new(FakeInner {
                blocks,
                canonical: vec![genesis.hash],
                info,
                max_range: None,
            })),
        }
    }

    pub fn genesis(&self) -> Header {
        self.header_sync(0)
    }

    pub fn header_sync(&self, n: u64) -> Header {
        let g = self.inner.lock().unwrap();
        g.blocks[&g.canonical[n as usize]].header.clone()
    }

    pub fn tip(&self) -> u64 {
        self.inner.lock().unwrap().canonical.len() as u64 - 1
    }

    /// Append a block. `logs` receives the block number; `deltas` are the scaled balances
    /// ((asset, account) -> (supply, debt, collateral)) that eth_call would return at this block.
    pub fn push(
        &self,
        salt: u8,
        logs: impl FnOnce(u64) -> Vec<RawLog>,
        deltas: DeltaSpec,
    ) -> Header {
        let mut g = self.inner.lock().unwrap();
        let number = g.canonical.len() as u64;
        let parent = *g.canonical.last().unwrap();
        let header = Header {
            number,
            hash: block_hash(number, salt),
            parent_hash: parent,
            timestamp: 1_700_000_000 + number * 12,
        };
        let d: Delta = deltas
            .into_iter()
            .map(|(k, (s, d, c))| (k, (U256::from(s), U256::from(d), c)))
            .collect();
        g.blocks.insert(
            header.hash,
            FakeBlock {
                header: header.clone(),
                logs: logs(number),
                deltas: d,
            },
        );
        g.canonical.push(header.hash);
        header
    }

    /// Simulate a reorg: keep only blocks <= height on the canonical chain.
    pub fn truncate_to(&self, height: u64) {
        self.inner
            .lock()
            .unwrap()
            .canonical
            .truncate(height as usize + 1);
    }

    pub fn set_info(&self, info: ProtocolInfo) {
        self.inner.lock().unwrap().info = info;
    }

    pub fn limit_range(&self, max_blocks: u64) {
        self.inner.lock().unwrap().max_range = Some(max_blocks);
    }
}

#[async_trait]
impl ChainSource for FakeChain {
    async fn head(&self) -> Result<u64> {
        Ok(self.tip())
    }
    async fn header(&self, number: u64) -> Result<Header> {
        let g = self.inner.lock().unwrap();
        let h = g
            .canonical
            .get(number as usize)
            .ok_or_else(|| anyhow!("no block {number}"))?;
        Ok(g.blocks[h].header.clone())
    }
    async fn logs_in_range(&self, from: u64, to: u64, _f: &LogFilter) -> Result<Vec<RawLog>> {
        let g = self.inner.lock().unwrap();
        if let Some(max) = g.max_range {
            if to - from + 1 > max {
                bail!("query returned more than 10000 results / range too large");
            }
        }
        let mut out = vec![];
        for n in from..=to {
            if let Some(h) = g.canonical.get(n as usize) {
                out.extend(g.blocks[h].logs.clone());
            }
        }
        Ok(out)
    }
    async fn logs_at_block(&self, hash: B256, _f: &LogFilter) -> Result<Vec<RawLog>> {
        let g = self.inner.lock().unwrap();
        Ok(g.blocks
            .get(&hash)
            .ok_or_else(|| anyhow!("unknown block"))?
            .logs
            .clone())
    }
    async fn read_positions(
        &self,
        hash: B256,
        keys: &[(Address, Address)],
        _t: &TokenMap,
    ) -> Result<Vec<PositionRead>> {
        let g = self.inner.lock().unwrap();
        let mut out = vec![];
        for (asset, account) in keys {
            let mut cur = hash;
            let (s, d, c) = loop {
                let b = g
                    .blocks
                    .get(&cur)
                    .ok_or_else(|| anyhow!("unknown block in walk"))?;
                if let Some(v) = b.deltas.get(&(*asset, *account)) {
                    break *v;
                }
                if b.header.number == 0 {
                    break (U256::ZERO, U256::ZERO, false);
                }
                cur = b.header.parent_hash;
            };
            out.push(PositionRead {
                asset: *asset,
                account: *account,
                scaled_supply: s,
                scaled_debt: d,
                stable_debt: U256::ZERO,
                collateral_enabled: c,
            });
        }
        Ok(out)
    }
    async fn load_protocol(&self) -> Result<ProtocolInfo> {
        Ok(self.inner.lock().unwrap().info.clone())
    }
}

// ---------------------------------------------------------------- MemStore

#[derive(Default)]
struct MemInner {
    processed: BTreeMap<u64, B256>,
    hist: BTreeMap<(Address, Address, u64), PositionHistoryRow>,
    pos: HashMap<(Address, Address), LatestPosition>,
    reserve_rows: BTreeMap<(Address, u64), U256>,
    liqs: BTreeSet<(u64, B256, u64)>,
    oracle_rows: BTreeSet<(u64, Address, U256, u64)>,
    emode_hist: BTreeMap<(Address, u64), u8>,
    tokens: Vec<ReserveTokens>,
}

#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<MemInner>>,
}

impl MemStore {
    pub fn reserve_index(&self, asset: Address, block: u64) -> Option<U256> {
        self.inner
            .lock()
            .unwrap()
            .reserve_rows
            .get(&(asset, block))
            .copied()
    }
    pub fn liquidation_count(&self) -> usize {
        self.inner.lock().unwrap().liqs.len()
    }
    pub fn oracle_update_count(&self) -> usize {
        self.inner.lock().unwrap().oracle_rows.len()
    }
    pub fn processed_blocks(&self) -> Vec<u64> {
        self.inner
            .lock()
            .unwrap()
            .processed
            .keys()
            .copied()
            .collect()
    }
}

#[async_trait]
impl Store for MemStore {
    async fn latest_processed(&self) -> Result<Option<(u64, B256)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .processed
            .iter()
            .next_back()
            .map(|(n, h)| (*n, *h)))
    }
    async fn processed_hash(&self, number: u64) -> Result<Option<B256>> {
        Ok(self.inner.lock().unwrap().processed.get(&number).copied())
    }
    async fn apply_block(&self, w: &BlockWrite) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let n = w.header.number;
        if w.record_block {
            g.processed.insert(n, w.header.hash);
        }
        for p in &w.positions {
            g.hist.insert(
                (p.asset, p.account, n),
                (
                    p.scaled_supply,
                    p.scaled_debt,
                    p.stable_debt,
                    p.collateral_enabled,
                ),
            );
            let newer_exists = g
                .pos
                .get(&(p.asset, p.account))
                .map(|(lb, ..)| *lb > n)
                .unwrap_or(false);
            if !newer_exists {
                g.pos.insert(
                    (p.asset, p.account),
                    (
                        n,
                        p.scaled_supply,
                        p.scaled_debt,
                        p.stable_debt,
                        p.collateral_enabled,
                    ),
                );
            }
        }
        for r in &w.reserves {
            g.reserve_rows.insert((r.asset, n), r.liquidity_index);
        }
        for l in &w.liquidations {
            g.liqs.insert((n, l.tx_hash, l.log_index));
        }
        for update in &w.oracle_updates {
            g.oracle_rows
                .insert((n, update.feed, update.round_id, update.log_index));
        }
        for (a, c) in &w.emodes {
            g.emode_hist.insert((*a, n), *c);
        }
        Ok(())
    }
    async fn put_checkpoint(&self, h: &Header) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .processed
            .insert(h.number, h.hash);
        Ok(())
    }
    async fn rollback_to(&self, number: u64) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let touched: BTreeSet<(Address, Address)> = g
            .hist
            .keys()
            .filter(|(_, _, b)| *b > number)
            .map(|(a, u, _)| (*a, *u))
            .collect();
        g.hist.retain(|(_, _, b), _| *b <= number);
        g.reserve_rows.retain(|(_, b), _| *b <= number);
        g.liqs.retain(|(b, _, _)| *b <= number);
        g.oracle_rows.retain(|(b, _, _, _)| *b <= number);
        g.emode_hist.retain(|(_, b), _| *b <= number);
        g.processed.retain(|b, _| *b <= number);
        for key in touched {
            g.pos.remove(&key);
            let latest = g
                .hist
                .iter()
                .filter(|((a, u, _), _)| (*a, *u) == key)
                .max_by_key(|((_, _, b), _)| *b)
                .map(|((_, _, b), v)| (*b, *v));
            if let Some((b, (s, d, stable, c))) = latest {
                g.pos.insert(key, (b, s, d, stable, c));
            }
        }
        Ok(())
    }
    async fn save_tokens(&self, reserves: &[ReserveTokens], _at: u64) -> Result<()> {
        self.inner.lock().unwrap().tokens = reserves.to_vec();
        Ok(())
    }
    async fn load_tokens(&self) -> Result<Vec<ReserveTokens>> {
        Ok(self.inner.lock().unwrap().tokens.clone())
    }
    async fn prune_processed(&self, below: u64) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .processed
            .retain(|b, _| *b >= below);
        Ok(())
    }
    async fn position(
        &self,
        asset: Address,
        account: Address,
    ) -> Result<Option<(u64, U256, U256, U256, bool)>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .pos
            .get(&(asset, account))
            .copied())
    }
}

#[derive(Clone, Default)]
pub struct RecordingEmitter {
    pub blocks: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl Emitter for RecordingEmitter {
    async fn block_processed(&self, _p: &str, _c: u64, h: &Header, _n: usize) {
        self.blocks.lock().unwrap().push(h.number);
    }
}
