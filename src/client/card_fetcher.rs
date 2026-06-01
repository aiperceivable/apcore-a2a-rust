//! AgentCardFetcher — TTL-cached remote Agent Card discovery.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

use super::exceptions::{A2AClientError, ClientResult};

/// Fetches and caches a remote agent's Agent Card.
///
/// Uses A2A 1.0's primary discovery path `/.well-known/agent-card.json` (the
/// server also serves the 0.3 `/.well-known/agent.json` alias). Results are
/// cached for `ttl`, keyed off a monotonic clock.
pub struct AgentCardFetcher {
    http: Client,
    url: String,
    ttl: Duration,
    cache: Mutex<Option<(Value, Instant)>>,
}

impl AgentCardFetcher {
    /// Default Agent Card cache TTL (300 seconds), matching Python/TS.
    pub const DEFAULT_TTL_SECS: u64 = 300;

    /// Construct a fetcher for `base_url` using the given HTTP client.
    pub fn new(http: Client, base_url: &str) -> Self {
        Self::with_ttl(http, base_url, Duration::from_secs(Self::DEFAULT_TTL_SECS))
    }

    /// Construct a fetcher with an explicit cache TTL.
    pub fn with_ttl(http: Client, base_url: &str, ttl: Duration) -> Self {
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );
        Self {
            http,
            url,
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// Fetch the Agent Card, returning the cached copy if still within TTL.
    ///
    /// Raises [`A2AClientError::Discovery`] on HTTP error or invalid JSON.
    pub async fn fetch(&self) -> ClientResult<Value> {
        if let Some((card, at)) = self.cache.lock().unwrap().as_ref() {
            if at.elapsed() < self.ttl {
                return Ok(card.clone());
            }
        }

        let resp = self
            .http
            .get(&self.url)
            .send()
            .await
            .map_err(|e| A2AClientError::Discovery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(A2AClientError::Discovery(format!(
                "Agent Card fetch returned HTTP {}",
                resp.status()
            )));
        }

        let card = resp
            .json::<Value>()
            .await
            .map_err(|e| A2AClientError::Discovery(format!("invalid Agent Card JSON: {e}")))?;

        *self.cache.lock().unwrap() = Some((card.clone(), Instant::now()));
        Ok(card)
    }
}
