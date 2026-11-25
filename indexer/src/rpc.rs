//! HTTP/WebSocket JSON-RPC ChainSource with retries, failover, request pacing, and header quorum.
use crate::abi::*;
use crate::chain::ChainSource;
use crate::metrics;
use crate::types::*;
use alloy::primitives::{Address, Bytes, B256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{BlockId, BlockNumberOrTag, Filter, TransactionInput, TransactionRequest};
use alloy::sol_types::SolCall;
use anyhow::{anyhow, bail, ensure, Context, Result};
use async_trait::async_trait;
use futures::{future::join_all, SinkExt, StreamExt};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

const KEYS_PER_MULTICALL: usize = 100;

pub struct RpcChain {
    providers: Vec<DynProvider>,
    addresses_provider: Address,
    websocket_url: Option<String>,
    preferred: AtomicUsize,
    max_attempts: usize,
    limiters: Vec<RateLimiter>,
    header_quorum: usize,
    data_provider: RwLock<Option<Address>>,
}

struct RateLimiter {
    interval: Duration,
    next: tokio::sync::Mutex<Instant>,
}

impl RateLimiter {
    fn new(requests_per_second: u64) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / requests_per_second as f64),
            next: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    async fn wait(&self) {
        let now = Instant::now();
        let mut next = self.next.lock().await;
        let due = (*next).max(now);
        *next = due + self.interval;
        drop(next);
        tokio::time::sleep(due.saturating_duration_since(now)).await;
    }
}

impl RpcChain {
    pub fn new(
        urls: &[String],
        addresses_provider: Address,
        websocket_url: Option<String>,
        requests_per_second: u64,
        header_quorum: usize,
    ) -> Result<Self> {
        if urls.is_empty() {
            bail!(
                "no RPC URL configured (set RPC_URL in .env; comma-separate several for failover)"
            );
        }
        ensure!(requests_per_second > 0, "RPC_RPS must be greater than zero");
        ensure!(
            header_quorum > 0 && header_quorum <= urls.len(),
            "RPC_QUORUM must be between 1 and the number of configured providers"
        );
        let providers = urls
            .iter()
            .map(|u| {
                let url = u.parse().context("invalid RPC URL")?;
                Ok(ProviderBuilder::new().connect_http(url).erased())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            providers,
            addresses_provider,
            websocket_url,
            preferred: AtomicUsize::new(0),
            max_attempts: urls.len().max(1) * 3,
            limiters: (0..urls.len())
                .map(|_| RateLimiter::new(requests_per_second))
                .collect(),
            header_quorum,
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
            self.limiters[idx].wait().await;
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

    async fn read_header(provider: DynProvider, number: u64) -> Result<Header> {
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await?
            .ok_or_else(|| anyhow!("block {number} not found (node behind?)"))?;
        Ok(Header {
            number: block.header.number,
            hash: block.header.hash,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        })
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

fn choose_header_quorum(number: u64, headers: Vec<Header>, quorum: usize) -> Result<Header> {
    let mut votes: std::collections::HashMap<B256, (Header, usize)> =
        std::collections::HashMap::new();
    for header in headers {
        let entry = votes
            .entry(header.hash)
            .or_insert_with(|| (header.clone(), 0));
        entry.1 += 1;
    }
    if let Some((header, _)) = votes
        .into_values()
        .filter(|(_, count)| *count >= quorum)
        .max_by_key(|(_, count)| *count)
    {
        return Ok(header);
    }
    bail!("RPC header quorum not reached for block {number} (required {quorum})")
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
        if self.header_quorum > 1 {
            let responses = join_all(self.providers.iter().cloned().enumerate().map(
                |(i, provider)| async move {
                    self.limiters[i].wait().await;
                    Self::read_header(provider, number).await
                },
            ))
            .await;
            return choose_header_quorum(
                number,
                responses.into_iter().filter_map(Result::ok).collect(),
                self.header_quorum,
            );
        }
        self.retry("eth_getBlockByNumber", |p| async move {
            Self::read_header(p, number).await
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
                    stable_debt: d.currentStableDebt,
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
        let oracle = getPriceOracleCall::abi_decode_returns(
            &self
                .eth_call(
                    ap,
                    getPriceOracleCall {}.abi_encode(),
                    latest,
                    "getPriceOracle",
                )
                .await?,
        )?;

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
        let mut oracle_feeds = std::collections::BTreeSet::new();
        for asset in list {
            let feed = getSourceOfAssetCall::abi_decode_returns(
                &self
                    .eth_call(
                        oracle,
                        getSourceOfAssetCall { asset }.abi_encode(),
                        latest,
                        "getSourceOfAsset",
                    )
                    .await?,
            )?;
            if feed != Address::ZERO {
                oracle_feeds.insert(feed);
            }
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
            oracle_feeds: oracle_feeds.into_iter().collect(),
        })
    }

    async fn subscribe_new_heads(
        &self,
    ) -> Result<Option<tokio::sync::mpsc::UnboundedReceiver<()>>> {
        let Some(url) = self.websocket_url.as_deref() else {
            return Ok(None);
        };
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|_| anyhow!("could not connect to configured WS_RPC_URL"))?;
        let (mut sink, mut stream) = socket.split();
        sink.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}"#.into(),
        ))
        .await
        .map_err(|_| anyhow!("failed to subscribe to WebSocket newHeads"))?;
        let response = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("WebSocket closed before eth_subscribe response"))?
            .map_err(|_| anyhow!("failed reading WebSocket eth_subscribe response"))?;
        let response = match response {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                serde_json::from_str::<serde_json::Value>(&text)
                    .context("invalid WebSocket eth_subscribe response")?
            }
            _ => bail!("unexpected WebSocket eth_subscribe response"),
        };
        let subscription = response["result"]
            .as_str()
            .context("WebSocket eth_subscribe did not return a subscription id")?
            .to_owned();

        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = stream.next().await {
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        let Ok(message) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if message["method"] == "eth_subscription"
                            && message["params"]["subscription"] == subscription
                            && sender.send(()).is_err()
                        {
                            break;
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Ping(payload)) => {
                        if sink
                            .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });
        Ok(Some(receiver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn header(hash: u8) -> Header {
        Header {
            number: 12,
            hash: B256::repeat_byte(hash),
            parent_hash: B256::ZERO,
            timestamp: 0,
        }
    }

    #[test]
    fn header_quorum_selects_the_matching_hash() {
        let selected = choose_header_quorum(12, vec![header(1), header(2), header(1)], 2).unwrap();
        assert_eq!(selected.hash, B256::repeat_byte(1));
    }

    #[test]
    fn header_quorum_rejects_disagreement() {
        assert!(choose_header_quorum(12, vec![header(1), header(2)], 2).is_err());
    }

    #[tokio::test]
    async fn websocket_subscription_notifies_on_new_head() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(socket).await.unwrap();
            let request = socket.next().await.unwrap().unwrap();
            assert!(matches!(request, Message::Text(text) if text.contains("eth_subscribe")));
            socket
                .send(Message::Text(
                    r#"{"jsonrpc":"2.0","id":1,"result":"sub-1"}"#.into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"sub-1","result":{"number":"0x1"}}}"#.into(),
                ))
                .await
                .unwrap();
        });
        let urls = vec!["http://127.0.0.1:8545".to_owned()];
        let chain =
            RpcChain::new(&urls, Address::ZERO, Some(format!("ws://{address}")), 1, 1).unwrap();
        let mut notifications = chain.subscribe_new_heads().await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }
}
