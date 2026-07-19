use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

use crate::state::SessionDescriptor;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub observed_at_ms: i64,
    pub session: SessionDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PriceSample {
    pub timestamp_ms: i64,
    pub session_slug: Option<String>,
    pub binance_btc_usd: Option<f64>,
    pub chainlink_btc_usd: Option<f64>,
    pub up_price: Option<f64>,
    pub down_price: Option<f64>,
}

pub struct HistoryStore {
    sessions_path: PathBuf,
    prices_path: PathBuf,
    seen_sessions: HashSet<String>,
}

impl HistoryStore {
    pub async fn open(root: &Path) -> Result<(Self, Vec<SessionRecord>, Vec<PriceSample>)> {
        tokio::fs::create_dir_all(root)
            .await
            .with_context(|| format!("failed creating data directory {}", root.display()))?;
        let sessions_path = root.join("sessions.ndjson");
        let prices_path = root.join("prices-1s.ndjson");
        let sessions = load_recent(&sessions_path, 500).await?;
        let prices = load_recent(&prices_path, 3_600).await?;
        let seen_sessions = sessions
            .iter()
            .map(|record: &SessionRecord| record.session.slug.clone())
            .collect();
        Ok((
            Self {
                sessions_path,
                prices_path,
                seen_sessions,
            },
            sessions,
            prices,
        ))
    }

    pub async fn record_session(&mut self, record: &SessionRecord) -> Result<bool> {
        if !self.seen_sessions.insert(record.session.slug.clone()) {
            return Ok(false);
        }
        append_json_line(&self.sessions_path, record).await?;
        Ok(true)
    }

    pub async fn record_price(&self, sample: &PriceSample) -> Result<()> {
        append_json_line(&self.prices_path, sample).await
    }
}

async fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value).context("failed serializing history row")?;
    line.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("failed opening history file {}", path.display()))?;
    file.write_all(&line)
        .await
        .with_context(|| format!("failed writing history file {}", path.display()))?;
    file.flush()
        .await
        .with_context(|| format!("failed flushing history file {}", path.display()))
}

async fn load_recent<T: DeserializeOwned>(path: &Path, cap: usize) -> Result<Vec<T>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading history file {}", path.display()));
        }
    };
    let mut rows = VecDeque::with_capacity(cap.min(1_024));
    let mut lines = BufReader::new(file).lines();
    let mut line_number = 0usize;
    while let Some(line) = lines
        .next_line()
        .await
        .with_context(|| format!("failed reading history file {}", path.display()))?
    {
        line_number += 1;
        if line.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<T>(&line) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    path = %path.display(),
                    line = line_number,
                    %error,
                    "skipping malformed history row"
                );
                continue;
            }
        };
        if rows.len() == cap {
            rows.pop_front();
        }
        rows.push_back(value);
    }
    Ok(rows.into())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{HistoryStore, PriceSample, SessionRecord};
    use crate::state::SessionDescriptor;

    fn session() -> SessionDescriptor {
        SessionDescriptor {
            slug: "btc-updown-5m-1".to_string(),
            title: "BTC Up or Down".to_string(),
            start_ms: 1_000,
            end_ms: 301_000,
            price_to_beat: Some(70_000.0),
            up_token_id: "up".to_string(),
            down_token_id: "down".to_string(),
            active: true,
            closed: false,
        }
    }

    #[tokio::test]
    async fn history_is_append_only_and_session_deduplicated() {
        let dir = tempdir().expect("tempdir");
        let (mut store, sessions, prices) = HistoryStore::open(dir.path()).await.expect("open");
        assert!(sessions.is_empty());
        assert!(prices.is_empty());
        let record = SessionRecord {
            observed_at_ms: 2_000,
            session: session(),
        };
        assert!(store.record_session(&record).await.expect("first"));
        assert!(!store.record_session(&record).await.expect("duplicate"));
        store
            .record_price(&PriceSample {
                timestamp_ms: 2_000,
                session_slug: Some(record.session.slug.clone()),
                binance_btc_usd: Some(70_001.0),
                chainlink_btc_usd: Some(70_000.0),
                up_price: Some(0.55),
                down_price: Some(0.45),
            })
            .await
            .expect("price");

        let (_, sessions, prices) = HistoryStore::open(dir.path()).await.expect("reopen");
        assert_eq!(sessions, vec![record]);
        assert_eq!(prices.len(), 1);
    }

    #[tokio::test]
    async fn malformed_trailing_history_does_not_block_restart() {
        let dir = tempdir().expect("tempdir");
        tokio::fs::write(dir.path().join("sessions.ndjson"), b"{not-json}\n")
            .await
            .expect("write malformed row");
        let (_, sessions, prices) = HistoryStore::open(dir.path()).await.expect("open");
        assert!(sessions.is_empty());
        assert!(prices.is_empty());
    }
}
