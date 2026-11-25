//! JSON-RPC implementation of ChainSource using alloy, with retry + provider failover.
use crate::abi::*;
use crate::chain::ChainSource;
use crate::metrics;
use crate::types::*;
use alloy::primitives::{Address, Bytes, B256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{BlockId, BlockNumberOrTag, Filter, TransactionInput, TransactionRequest};
use alloy::sol_types::SolCall;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;
use std::time::Duration;

const KEYS_PER_MULTICALL: usize = 100;

pub struct RpcChain {
    providers: Vec<DynProvider>,
    addresses_provider: Address,
    preferred: AtomicUsize,
    max_attempts: usize,
    data_provider: RwLock<Option<Address>>,
}

impl RpcChain {
    pub fn new(urls: &[String], addresses_provider: Address) -> Result<Self> {
        if urls.is_empty() {
            bail!(
                "no RPC URL configured (set RPC_URL in .env; comma-separate several for failover)"
            );
        }
        let providers = urls
            .iter()
            .map(|u| {
                let url = u.parse().with_context(|| format!("invalid RPC URL: {u}"))?;
                Ok(ProviderBuilder::new().connect_http(url).erased())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            providers,
            addresses_provider,
            preferred: AtomicUsize::new(0),
            max_attempts: urls.len().max(1) * 3,
            data_provider: RwLock::new(None),
        })
    }

    /// Run `f` against providers with exponential backoff, rotating to the next provider on failure.
    async fn retry<T, F, Fut>(&self, what: &str, f: F) -> Result<T>
    where
        F: Fn(DynProvider) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let n = self.providers.len();
        let start = self.preferred.load(Ordering::Relaxed);
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.max_attempts {
            let idx = (start + attempt) % n;
            match f(self.providers[idx].clone()).await {
                Ok(v) => {
                    self.preferred.store(idx, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(e) => {
                    metrics::RPC_ERRORS.inc();
                    tracing::warn!(what, provider = idx, attempt, error = %e, "rpc call failed");
                    last = Some(e);
                    let delay = Duration::from_millis((200u64 << attempt.min(5)).min(5_000));
                    tokio::time::sleep(delay).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no attempts made")))
            .with_context(|| format!("rpc `{what}` failed after retries"))
    }

    async fn eth_call(
        &self,
        to: Address,
        data: Vec<u8>,
        block: BlockId,
        what: &str,
    ) -> Result<Bytes> {
        self.retry(what, |p| {
            let data = data.clone();
            async move {
                let req = TransactionRequest::default()
                    .to(to)
                    .input(TransactionInput::new(Bytes::from(data)));
                Ok(p.call(req).block(block).await?)
            }
        })
        .await
    }

    fn data_provider(&self) -> Result<Address> {
        self.data_provider
            .read()
            .unwrap()
            .ok_or_else(|| anyhow!("data provider not resolved yet: call load_protocol() first"))
    }
}

fn to_raw(log: &alloy::rpc::types::Log) -> Result<RawLog> {
    Ok(RawLog {
        address: log.address(),
        topics: log.topics().to_vec(),
        data: log.data().data.clone(),
        block_number: log
            .block_number
            .context("log without block_number (pending block?)")?,
        tx_hash: log.transaction_hash.unwrap_or_default(),
        log_index: log.log_index.context("log without log_index")?,
    })
}

fn apply_filter(mut f: Filter, filter: &LogFilter) -> Filter {
    if !filter.addresses.is_empty() {
        f = f.address(filter.addresses.clone());
    }
    if !filter.topics0.is_empty() {
        f = f.event_signature(filter.topics0.clone());
    }
    f
}

#[async_trait]
impl ChainSource for RpcChain {
    async fn head(&self) -> Result<u64> {
        self.retry("eth_blockNumber", |p| async move {
            Ok(p.get_block_number().await?)
        })
        .await
    }

    async fn header(&self, number: u64) -> Result<Header> {
        self.retry("eth_getBlockByNumber", |p| async move {
            let b = p
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await?
                .ok_or_else(|| anyhow!("block {number} not found (node behind?)"))?;
            Ok(Header {
                number: b.header.number,
                hash: b.header.hash,
                parent_hash: b.header.parent_hash,
                timestamp: b.header.timestamp,
            })
        })
        .await
    }

    async fn logs_in_range(&self, from: u64, to: u64, filter: &LogFilter) -> Result<Vec<RawLog>> {
        let logs = self
            .retry("eth_getLogs(range)", |p| async move {
                let f = apply_filter(Filter::new().from_block(from).to_block(to), filter);
                Ok(p.get_logs(&f).await?)
            })
            .await?;
        logs.iter().map(to_raw).collect()
    }

    async fn logs_at_block(&self, hash: B256, filter: &LogFilter) -> Result<Vec<RawLog>> {
        let logs = self
            .retry("eth_getLogs(blockHash)", |p| async move {
                let f = apply_filter(Filter::new().at_block_hash(hash), filter);
                Ok(p.get_logs(&f).await?)
            })
            .await?;
        logs.iter().map(to_raw).collect()
    }

    async fn read_positions(
        &self,
        hash: B256,
        keys: &[(Address, Address)],
        tokens: &TokenMap,
    ) -> Result<Vec<PositionRead>> {
        let dp = self.data_provider()?;
        let block = BlockId::hash(hash);
        let mut out = Vec::with_capacity(keys.len());

        for batch in keys.chunks(KEYS_PER_MULTICALL) {
            let mut calls: Vec<Call3> = Vec::with_capacity(batch.len() * 2);
            for (asset, account) in batch {
                let t = tokens
                    .get(asset)
                    .ok_or_else(|| anyhow!("unknown reserve {asset}"))?;
                calls.push(Call3 {
                    target: t.atoken,
                    allowFailure: false,
                    callData: scaledBalanceOfCall { user: *account }.abi_encode().into(),
                });
                calls.push(Call3 {
                    target: dp,
                    allowFailure: false,
                    callData: getUserReserveDataCall {
                        asset: *asset,
                        user: *account,
                    }
                    .abi_encode()
                    .into(),
                });
            }
            let raw = self
                .eth_call(
                    MULTICALL3,
                    aggregate3Call { calls }.abi_encode(),
                    block,
                    "multicall3.aggregate3",
                )
                .await?;
            let results =
                aggregate3Call::abi_decode_returns(&raw).context("decoding multicall3 result")?;
            if results.len() != batch.len() * 2 {
                bail!(
                    "multicall returned {} results, expected {}",
                    results.len(),
                    batch.len() * 2
                );
            }
            for (i, (asset, account)) in batch.iter().enumerate() {
                let (r_bal, r_data) = (&results[2 * i], &results[2 * i + 1]);
                if !r_bal.success || !r_data.success {
                    bail!("call failed for {account} on {asset} at block {hash}");
                }
                let scaled_supply = scaledBalanceOfCall::abi_decode_returns(&r_bal.returnData)?;
                let d = getUserReserveDataCall::abi_decode_returns(&r_data.returnData)?;
                out.push(PositionRead {
                    asset: *asset,
                    account: *account,
                    scaled_supply,
                    scaled_debt: d.scaledVariableDebt,
                    collateral_enabled: d.usageAsCollateralEnabled,
                });
            }
        }
        Ok(out)
    }

    async fn load_protocol(&self) -> Result<ProtocolInfo> {
        let latest = BlockId::latest();
        let ap = self.addresses_provider;

        let pool = getPoolCall::abi_decode_returns(
            &self
                .eth_call(ap, getPoolCall {}.abi_encode(), latest, "getPool")
                .await?,
        )?;
        let dp = getPoolDataProviderCall::abi_decode_returns(
            &self
                .eth_call(
                    ap,
                    getPoolDataProviderCall {}.abi_encode(),
                    latest,
                    "getPoolDataProvider",
                )
                .await?,
        )?;
        *self.data_provider.write().unwrap() = Some(dp);

        let list = getReservesListCall::abi_decode_returns(
            &self
                .eth_call(
                    pool,
                    getReservesListCall {}.abi_encode(),
                    latest,
                    "getReservesList",
                )
                .await?,
        )?;

        let mut reserves = Vec::with_capacity(list.len());
        for asset in list {
            let t = getReserveTokensAddressesCall::abi_decode_returns(
                &self
                    .eth_call(
                        dp,
                        getReserveTokensAddressesCall { asset }.abi_encode(),
                        latest,
                        "getReserveTokensAddresses",
                    )
                    .await?,
            )?;
            // symbol/decimals are for humans; tolerate non-standard tokens (e.g. bytes32 symbols).
            let symbol = match self
                .eth_call(asset, symbolCall {}.abi_encode(), latest, "symbol")
                .await
            {
                Ok(b) => {
                    symbolCall::abi_decode_returns(&b).unwrap_or_else(|_| format!("{asset:#x}"))
                }
                Err(_) => format!("{asset:#x}"),
            };
            let decimals = match self
                .eth_call(asset, decimalsCall {}.abi_encode(), latest, "decimals")
                .await
            {
                Ok(b) => decimalsCall::abi_decode_returns(&b).unwrap_or(18),
                Err(_) => 18,
            };
            reserves.push(ReserveTokens {
                asset,
                atoken: t.aTokenAddress,
                variable_debt_token: t.variableDebtTokenAddress,
                symbol,
                decimals,
            });
        }
        Ok(ProtocolInfo {
            pool,
            data_provider: dp,
            reserves,
        })
    }
}
