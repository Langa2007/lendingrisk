//! Tiny Prometheus-text exporter (no extra dependencies). Metrics are process-wide atomics.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Counter(AtomicU64);
impl Counter {
    pub const fn new() -> Self {
        Counter(AtomicU64::new(0))
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }
    pub fn set(&self, n: u64) {
        self.0.store(n, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}
impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

pub static HEAD_BLOCK: Counter = Counter::new();
pub static CURSOR_BLOCK: Counter = Counter::new();
pub static BLOCKS_PROCESSED: Counter = Counter::new();
pub static EVENTS_DECODED: Counter = Counter::new();
pub static POSITIONS_WRITTEN: Counter = Counter::new();
pub static REORGS: Counter = Counter::new();
pub static MAX_REORG_DEPTH: Counter = Counter::new();
pub static RPC_ERRORS: Counter = Counter::new();
pub static EMIT_ERRORS: Counter = Counter::new();
pub static LAST_PROCESSED_UNIX: Counter = Counter::new();

pub fn render() -> String {
    let head = HEAD_BLOCK.get();
    let cur = CURSOR_BLOCK.get();
    let lag = head.saturating_sub(cur);
    let mut s = String::new();
    let mut m = |name: &str, help: &str, kind: &str, v: u64| {
        s.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}\n"
        ));
    };
    m(
        "tahadhari_indexer_head_block",
        "Latest chain head seen",
        "gauge",
        head,
    );
    m(
        "tahadhari_indexer_cursor_block",
        "Last block fully processed",
        "gauge",
        cur,
    );
    m(
        "tahadhari_indexer_lag_blocks",
        "Head minus cursor",
        "gauge",
        lag,
    );
    m(
        "tahadhari_indexer_blocks_processed_total",
        "Blocks processed",
        "counter",
        BLOCKS_PROCESSED.get(),
    );
    m(
        "tahadhari_indexer_events_decoded_total",
        "Position touches decoded",
        "counter",
        EVENTS_DECODED.get(),
    );
    m(
        "tahadhari_indexer_positions_written_total",
        "Position rows written",
        "counter",
        POSITIONS_WRITTEN.get(),
    );
    m(
        "tahadhari_indexer_reorgs_total",
        "Reorgs handled",
        "counter",
        REORGS.get(),
    );
    m(
        "tahadhari_indexer_max_reorg_depth",
        "Deepest reorg handled",
        "gauge",
        MAX_REORG_DEPTH.get(),
    );
    m(
        "tahadhari_indexer_rpc_errors_total",
        "RPC call failures (before failover)",
        "counter",
        RPC_ERRORS.get(),
    );
    m(
        "tahadhari_indexer_emit_errors_total",
        "Redis emit failures",
        "counter",
        EMIT_ERRORS.get(),
    );
    m(
        "tahadhari_indexer_last_processed_unix",
        "Unix time of last processed block",
        "gauge",
        LAST_PROCESSED_UNIX.get(),
    );
    s
}

/// Serve `/metrics` on `addr` forever. Intentionally minimal; bind to localhost.
pub async fn serve(addr: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "metrics endpoint listening");
    loop {
        let (mut sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = render();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}
