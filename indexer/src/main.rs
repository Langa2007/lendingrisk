//! Tahadhari indexer CLI.
//!   indexer backfill --from <block> [--to <block>]   historical range (confirmed blocks only)
//!   indexer run [--start-block <block>]              catch up, then tail the chain forever
//!   indexer status                                   cursor vs head
mod abi;
mod chain;
mod decode;
mod emit;
mod indexer;
mod metrics;
mod rpc;
mod state;
mod store;
#[cfg(test)]
mod testutil;
mod types;

use anyhow::{Context, Result};
use chain::ChainSource;
use clap::{Args, Parser, Subcommand};
use emit::{Emitter, NoopEmitter, RedisEmitter};
use indexer::{Indexer, IndexerConfig};
use std::str::FromStr;
use std::time::Duration;
use store::{PgStore, Store};
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    name = "indexer",
    version,
    about = "Tahadhari: Aave v3 position and reserve indexer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    #[command(flatten)]
    opts: Opts,
}

#[derive(Args)]
struct Opts {
    /// One or more JSON-RPC URLs, comma separated (later ones are failover providers)
    #[arg(long, env = "RPC_URL", value_delimiter = ',')]
    rpc_url: Vec<String>,
    /// Optional extra failover provider
    #[arg(long, env = "RPC_URL_BACKUP")]
    rpc_url_backup: Option<String>,
    /// Maximum JSON-RPC requests per second per provider
    #[arg(long, env = "RPC_RPS", default_value_t = 10)]
    rpc_rps: u64,
    /// Matching providers required for each canonical block header
    #[arg(long, env = "RPC_QUORUM", default_value_t = 1)]
    rpc_quorum: usize,
    /// Optional WebSocket endpoint for new-head notifications
    #[arg(long, env = "WS_RPC_URL")]
    ws_rpc_url: Option<String>,
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// If set, emit stream:block_processed messages
    #[arg(long, env = "REDIS_URL")]
    redis_url: Option<String>,
    #[arg(
        long,
        env = "AAVE_V3_ADDRESSES_PROVIDER",
        default_value = "0x2f39d218133AFaB8F2B819B1066c7E434Ad94E9e"
    )]
    addresses_provider: String,
    /// Optional first indexed block for a fresh database; set to the deployment block for full history
    #[arg(long, env = "AAVE_V3_START_BLOCK")]
    aave_v3_start_block: Option<u64>,
    #[arg(long, env = "CHAIN_ID", default_value_t = 1)]
    chain_id: u64,
    /// Backfill stops this many blocks below head
    #[arg(long, env = "CONFIRMATIONS", default_value_t = 12)]
    confirmations: u64,
    /// eth_getLogs range per request (shrinks automatically on provider limits)
    #[arg(long, env = "CHUNK_SIZE", default_value_t = 1000)]
    chunk_size: u64,
    #[arg(long, env = "POLL_MS", default_value_t = 2000)]
    poll_ms: u64,
    /// Deepest reorg repaired automatically
    #[arg(long, env = "REORG_DEPTH", default_value_t = 64)]
    reorg_depth: u64,
    #[arg(long, env = "METRICS_ADDR", default_value = "127.0.0.1:9101")]
    metrics_addr: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a historical block range
    Backfill {
        #[arg(long)]
        from: u64,
        /// Defaults to head minus CONFIRMATIONS
        #[arg(long)]
        to: Option<u64>,
        /// Allow indexing blocks that are not yet confirmed (reorg-unsafe)
        #[arg(long)]
        unsafe_tip: bool,
    },
    /// Catch up and follow the chain. A fresh database needs --start-block or AAVE_V3_START_BLOCK.
    Run {
        #[arg(long)]
        start_block: Option<u64>,
    },
    /// Print cursor, head and lag
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let o = cli.opts;
    let aave_v3_start_block = o.aave_v3_start_block;
    let ws_rpc_url = o.ws_rpc_url;

    let mut urls: Vec<String> = o
        .rpc_url
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if let Some(b) = o.rpc_url_backup.filter(|s| !s.trim().is_empty()) {
        urls.push(b);
    }
    let ap = alloy::primitives::Address::from_str(&o.addresses_provider)
        .context("AAVE_V3_ADDRESSES_PROVIDER is not an address")?;
    let chain = rpc::RpcChain::new(&urls, ap, ws_rpc_url, o.rpc_rps, o.rpc_quorum)?;
    let store = PgStore::connect(&o.database_url, "aave_v3", o.chain_id).await?;

    let emitter: Box<dyn Emitter> = match o.redis_url.filter(|s| !s.trim().is_empty()) {
        Some(url) => match RedisEmitter::connect(&url).await {
            Ok(e) => Box::new(e),
            Err(e) => {
                warn!(error = %e, "redis unavailable: continuing without block_processed notifications");
                Box::new(NoopEmitter)
            }
        },
        None => Box::new(NoopEmitter),
    };

    let cfg = IndexerConfig {
        chain_id: o.chain_id,
        confirmations: o.confirmations,
        chunk_size: o.chunk_size,
        poll: Duration::from_millis(o.poll_ms),
        reorg_depth: o.reorg_depth,
        ..Default::default()
    };
    let confirmations = cfg.confirmations;
    let mut idx = Indexer::new(chain, store, emitter, cfg);

    match cli.cmd {
        Cmd::Status => {
            let head = idx.chain.head().await?;
            match idx.store.latest_processed().await? {
                Some((n, h)) => println!(
                    "cursor block {n} ({h:#x})\nhead   block {head}\nlag    {} blocks",
                    head.saturating_sub(n)
                ),
                None => println!(
                    "store is empty (head is {head}); run `indexer backfill --from <block>`"
                ),
            }
        }
        Cmd::Backfill {
            from,
            to,
            unsafe_tip,
        } => {
            idx.init().await?;
            let head = idx.chain.head().await?;
            let safe = head.saturating_sub(confirmations);
            let mut to = to.unwrap_or(safe);
            if to > safe && !unsafe_tip {
                warn!(
                    requested = to,
                    clamped = safe,
                    "clamping --to to head - CONFIRMATIONS (use --unsafe-tip to override)"
                );
                to = safe;
            }
            anyhow::ensure!(from <= to, "--from ({from}) must be <= --to ({to})");
            info!(from, to, "backfill starting");
            idx.backfill(from, to).await?;
            info!("backfill complete");
        }
        Cmd::Run { start_block } => {
            tokio::spawn(metrics::serve(o.metrics_addr));
            idx.init().await?;
            if idx.store.latest_processed().await?.is_none() {
                let s = start_block.or(aave_v3_start_block).context(
                    "database is empty: pass --start-block <block> or set AAVE_V3_START_BLOCK",
                )?;
                idx.seed(s).await?;
            } else if let Some(s) = start_block {
                warn!(
                    start_block = s,
                    "ignored: database already has a cursor; resuming from it"
                );
            }
            idx.run_live().await?;
        }
    }
    Ok(())
}
