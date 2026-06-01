//! AgentCardFetcher — discover remote agent cards.

use reqwest::Client;
use serde_json::Value;

pub struct AgentCardFetcher {
    http: Client,
}

impl AgentCardFetcher {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }

    pub async fn fetch(&self, base_url: &str) -> Result<Value, reqwest::Error> {
        // A2A 1.0 primary discovery path (servers also serve the 0.3 alias).
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );
        self.http.get(&url).send().await?.json::<Value>().await
    }
}

impl Default for AgentCardFetcher {
    fn default() -> Self {
        Self::new()
    }
}
