//! Notifies the rest of the system (Python engine, Go services) that a block was processed.
//! Postgres is the source of truth; the stream is a wake-up signal. Emit failures never stop indexing.
use crate::types::*;
use async_trait::async_trait;
use serde_json::json;

pub const STREAM_BLOCK_PROCESSED: &str = "stream:block_processed";
pub const STREAM_ORACLE_UPDATE: &str = "stream:oracle_update";

#[async_trait]
pub trait Emitter: Send + Sync {
    async fn block_processed(
        &self,
        protocol: &str,
        chain_id: u64,
        h: &Header,
        positions_changed: usize,
    );
    async fn oracle_update(
        &self,
        _protocol: &str,
        _chain_id: u64,
        _h: &Header,
        _update: &OracleUpdate,
    ) {
    }
}

pub struct NoopEmitter;
#[async_trait]
impl Emitter for NoopEmitter {
    async fn block_processed(&self, _: &str, _: u64, _: &Header, _: usize) {}
}

/// Builds a `block_processed.v1` message (schemas/json/block_processed.v1.json).
pub fn block_processed_message(
    protocol: &str,
    chain_id: u64,
    h: &Header,
    positions_changed: usize,
    trace_id: &str,
) -> String {
    json!({
        "schema_version": 1,
        "trace_id": trace_id,
        "protocol": protocol,
        "chain_id": chain_id,
        "block_number": h.number,
        "block_hash": hash_str(&h.hash),
        "ts": iso8601(h.timestamp),
        "positions_changed": positions_changed,
    })
    .to_string()
}

pub fn oracle_update_message(
    protocol: &str,
    chain_id: u64,
    h: &Header,
    update: &OracleUpdate,
    trace_id: &str,
) -> String {
    json!({
        "schema_version": 1,
        "trace_id": trace_id,
        "protocol": protocol,
        "chain_id": chain_id,
        "feed": addr_str(&update.feed),
        "block_number": h.number,
        "round_id": update.round_id.to_string(),
        "answer": update.answer,
        "updated_at": iso8601(update.updated_at),
        "ts": iso8601(h.timestamp),
        "log_index": update.log_index,
    })
    .to_string()
}

/// Unix seconds -> "YYYY-MM-DDTHH:MM:SSZ" (UTC) without a date library.
pub fn iso8601(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub struct RedisEmitter {
    conn: redis::aio::ConnectionManager,
    maxlen: usize,
}

impl RedisEmitter {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            maxlen: 10_000,
        })
    }
}

#[async_trait]
impl Emitter for RedisEmitter {
    async fn block_processed(
        &self,
        protocol: &str,
        chain_id: u64,
        h: &Header,
        positions_changed: usize,
    ) {
        let trace = format!("{chain_id}-{}-{}", h.number, &hash_str(&h.hash)[2..10]);
        let msg = block_processed_message(protocol, chain_id, h, positions_changed, &trace);
        let mut conn = self.conn.clone();
        let res: redis::RedisResult<String> = redis::cmd("XADD")
            .arg(STREAM_BLOCK_PROCESSED)
            .arg("MAXLEN")
            .arg("~")
            .arg(self.maxlen)
            .arg("*")
            .arg("data")
            .arg(msg)
            .query_async(&mut conn)
            .await;
        if let Err(e) = res {
            crate::metrics::EMIT_ERRORS.inc();
            tracing::warn!(error = %e, "redis XADD failed (indexing continues)");
        }
    }

    async fn oracle_update(
        &self,
        protocol: &str,
        chain_id: u64,
        h: &Header,
        update: &OracleUpdate,
    ) {
        let trace = format!("{chain_id}-{}-{}", h.number, update.log_index);
        let msg = oracle_update_message(protocol, chain_id, h, update, &trace);
        let mut conn = self.conn.clone();
        let res: redis::RedisResult<String> = redis::cmd("XADD")
            .arg(STREAM_ORACLE_UPDATE)
            .arg("MAXLEN")
            .arg("~")
            .arg(self.maxlen)
            .arg("*")
            .arg("data")
            .arg(msg)
            .query_async(&mut conn)
            .await;
        if let Err(e) = res {
            crate::metrics::EMIT_ERRORS.inc();
            tracing::warn!(error = %e, "redis oracle XADD failed (indexing continues)");
        }
    }
}

#[async_trait]
impl Emitter for Box<dyn Emitter> {
    async fn block_processed(
        &self,
        protocol: &str,
        chain_id: u64,
        h: &Header,
        positions_changed: usize,
    ) {
        (**self)
            .block_processed(protocol, chain_id, h, positions_changed)
            .await
    }

    async fn oracle_update(
        &self,
        protocol: &str,
        chain_id: u64,
        h: &Header,
        update: &OracleUpdate,
    ) {
        (**self).oracle_update(protocol, chain_id, h, update).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    #[test]
    fn iso8601_known_values() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z"); // leap day
    }

    #[test]
    fn message_matches_schema_fields() {
        let h = Header {
            number: 7,
            hash: B256::repeat_byte(1),
            parent_hash: B256::repeat_byte(2),
            timestamp: 1_700_000_000,
        };
        let v: serde_json::Value =
            serde_json::from_str(&block_processed_message("aave_v3", 1, &h, 3, "t")).unwrap();
        for k in [
            "schema_version",
            "trace_id",
            "protocol",
            "chain_id",
            "block_number",
            "block_hash",
            "ts",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["block_number"], 7);
    }

    #[test]
    fn oracle_message_contains_schema_fields() {
        let h = Header {
            number: 7,
            hash: B256::repeat_byte(1),
            parent_hash: B256::repeat_byte(2),
            timestamp: 1_700_000_000,
        };
        let update = OracleUpdate {
            feed: alloy::primitives::Address::repeat_byte(3),
            round_id: alloy::primitives::U256::from(4u64),
            answer: "12345".into(),
            updated_at: 1_700_000_000,
            log_index: 8,
        };
        let v: serde_json::Value =
            serde_json::from_str(&oracle_update_message("aave_v3", 1, &h, &update, "t")).unwrap();
        for key in [
            "schema_version",
            "trace_id",
            "chain_id",
            "feed",
            "block_number",
            "round_id",
            "answer",
            "updated_at",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["answer"], "12345");
    }
}
