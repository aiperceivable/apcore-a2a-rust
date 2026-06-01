//! A2A client for calling remote A2A agents.

use reqwest::Client;
use serde_json::{json, Value};

pub struct A2AClient {
    http: Client,
    base_url: String,
}

impl A2AClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into(),
        }
    }

    pub async fn send_message(
        &self,
        skill_id: &str,
        message: &str,
    ) -> Result<Value, reqwest::Error> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "role": "ROLE_USER",
                    "parts": [{ "text": message }]
                },
                "metadata": { "skillId": skill_id }
            }
        });

        let resp = self
            .http
            .post(&self.base_url)
            .json(&body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        Ok(resp)
    }
}
