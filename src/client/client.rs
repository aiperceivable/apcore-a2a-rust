//! A2AClient — HTTP client for calling remote A2A 1.0 agents.
//!
//! Implements the F-07 client surface (Python/TS parity): message/send,
//! message/stream (SSE), tasks/get|cancel|list, and TTL-cached Agent Card
//! discovery. Errors map to the typed [`A2AClientError`] hierarchy.

use std::time::Duration;

use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;

use super::card_fetcher::AgentCardFetcher;
use super::exceptions::{A2AClientError, ClientResult};

/// Terminal A2A 1.0 task states; streaming stops when one is observed.
const TERMINAL_STATES: [&str; 4] = [
    "TASK_STATE_COMPLETED",
    "TASK_STATE_FAILED",
    "TASK_STATE_CANCELED",
    "TASK_STATE_REJECTED",
];

/// Default HTTP request timeout (seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// HTTP client for a single remote A2A agent.
pub struct A2AClient {
    http: Client,
    /// Base URL with any trailing slash stripped.
    base_url: String,
    fetcher: AgentCardFetcher,
}

impl A2AClient {
    /// Construct a client for `url` with default timeout/TTL and no auth.
    ///
    /// # Panics
    /// Use [`A2AClient::try_new`] to handle invalid URLs without panicking.
    pub fn new(url: impl Into<String>) -> Self {
        Self::try_new(url, None, None, None).expect("invalid A2A agent URL")
    }

    /// Construct a client, validating the URL and applying optional auth bearer
    /// token, request timeout, and Agent Card cache TTL.
    ///
    /// `auth` is the full `Authorization` header value (e.g. `"Bearer eyJ..."`).
    pub fn try_new(
        url: impl Into<String>,
        auth: Option<String>,
        timeout: Option<Duration>,
        card_ttl: Option<Duration>,
    ) -> ClientResult<Self> {
        let url = url.into();
        validate_url(&url)?;
        let base_url = url.trim_end_matches('/').to_string();

        let mut builder = Client::builder()
            .timeout(timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS)));
        if let Some(auth) = auth {
            let mut headers = reqwest::header::HeaderMap::new();
            let value = reqwest::header::HeaderValue::from_str(&auth)
                .map_err(|e| A2AClientError::InvalidUrl(format!("invalid auth header: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        let http = builder
            .build()
            .map_err(|e| A2AClientError::Connection(e.to_string()))?;

        let fetcher = match card_ttl {
            Some(ttl) => AgentCardFetcher::with_ttl(http.clone(), &base_url, ttl),
            None => AgentCardFetcher::new(http.clone(), &base_url),
        };

        Ok(Self {
            http,
            base_url,
            fetcher,
        })
    }

    /// Fetch and TTL-cache the remote Agent Card.
    pub async fn agent_card(&self) -> ClientResult<Value> {
        self.fetcher.fetch().await
    }

    /// Convenience alias for [`A2AClient::agent_card`].
    pub async fn discover(&self) -> ClientResult<Value> {
        self.agent_card().await
    }

    /// Send a `message/send` JSON-RPC request and return the resulting Task.
    pub async fn send_message(
        &self,
        message: Value,
        metadata: Option<Value>,
        context_id: Option<String>,
    ) -> ClientResult<Value> {
        let params = build_message_params(message, metadata, context_id);
        self.jsonrpc_call("message/send", params).await
    }

    /// Send a `message/stream` request and yield parsed SSE event objects.
    ///
    /// The stream terminates when the server closes the connection or a terminal
    /// `TASK_STATE_*` status is observed (A2A 1.0 has no `final` flag).
    pub fn stream_message(
        &self,
        message: Value,
        metadata: Option<Value>,
        context_id: Option<String>,
    ) -> impl Stream<Item = ClientResult<Value>> + '_ {
        let params = build_message_params(message, metadata, context_id);
        let body = json!({
            "jsonrpc": "2.0",
            "id": Uuid::new_v4().to_string(),
            "method": "message/stream",
            "params": params,
        });
        let url = format!("{}/", self.base_url);
        let http = self.http.clone();

        try_stream! {
            let resp = http
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| A2AClientError::Connection(e.to_string()))?;

            if !resp.status().is_success() {
                Err(A2AClientError::Connection(format!(
                    "stream returned HTTP {}",
                    resp.status()
                )))?;
            }

            let mut byte_stream = resp.bytes_stream();
            let mut buf = String::new();
            'outer: while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|e| A2AClientError::Connection(e.to_string()))?;
                buf.push_str(&String::from_utf8_lossy(&chunk));

                // SSE events are separated by blank lines; parse complete lines.
                while let Some(nl) = buf.find('\n') {
                    let line: String = buf.drain(..=nl).collect();
                    let line = line.trim_end();
                    // Skip keepalive comment lines (": ...") and blank separators.
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim_start();
                        if let Ok(event) = serde_json::from_str::<Value>(data) {
                            let terminal = is_terminal_event(&event);
                            yield event;
                            if terminal {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Retrieve task state via `tasks/get`.
    pub async fn get_task(&self, task_id: &str) -> ClientResult<Value> {
        self.jsonrpc_call("tasks/get", json!({ "id": task_id }))
            .await
            .map_err(|e| attach_task_id(e, task_id))
    }

    /// Cancel a task via `tasks/cancel`.
    pub async fn cancel_task(&self, task_id: &str) -> ClientResult<Value> {
        self.jsonrpc_call("tasks/cancel", json!({ "id": task_id }))
            .await
            .map_err(|e| attach_task_id(e, task_id))
    }

    /// List tasks via `tasks/list`.
    pub async fn list_tasks(
        &self,
        context_id: Option<String>,
        limit: i64,
    ) -> ClientResult<Value> {
        let mut params = json!({ "limit": limit });
        if let Some(cid) = context_id {
            params["contextId"] = Value::String(cid);
        }
        self.jsonrpc_call("tasks/list", params).await
    }

    /// Close the client (releases the underlying HTTP connection pool).
    ///
    /// Provided for parity with Python/TS; the pool is also dropped when the
    /// client goes out of scope.
    pub async fn close(self) {}

    /// POST a JSON-RPC request, returning the `result` or a typed error.
    async fn jsonrpc_call(&self, method: &str, params: Value) -> ClientResult<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": Uuid::new_v4().to_string(),
            "method": method,
            "params": params,
        });
        let url = format!("{}/", self.base_url);

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| A2AClientError::Connection(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(A2AClientError::Connection(format!(
                "HTTP {}",
                resp.status()
            )));
        }

        let data: Value = resp
            .json()
            .await
            .map_err(|e| A2AClientError::Connection(format!("invalid JSON response: {e}")))?;

        if let Some(err) = data.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603) as i32;
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Err(A2AClientError::from_jsonrpc(code, message));
        }

        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Build the JSON-RPC params object for message/send and message/stream.
fn build_message_params(
    message: Value,
    metadata: Option<Value>,
    context_id: Option<String>,
) -> Value {
    let mut params = json!({
        "message": message,
        "metadata": metadata.unwrap_or_else(|| json!({})),
    });
    if let Some(cid) = context_id {
        params["contextId"] = Value::String(cid);
    }
    params
}

/// True if an SSE event carries a terminal task status.
fn is_terminal_event(event: &Value) -> bool {
    event
        .get("statusUpdate")
        .and_then(|su| su.get("status"))
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(|state| TERMINAL_STATES.contains(&state))
        .unwrap_or(false)
}

/// Attach the task id to a bare TaskNotFound error for better diagnostics.
fn attach_task_id(err: A2AClientError, task_id: &str) -> A2AClientError {
    match err {
        A2AClientError::TaskNotFound { task_id: None } => A2AClientError::TaskNotFound {
            task_id: Some(task_id.to_string()),
        },
        other => other,
    }
}

/// Validate that `url` is a well-formed http/https URL.
fn validate_url(url: &str) -> ClientResult<()> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    match rest {
        Some(authority) if !authority.is_empty() && !authority.starts_with('/') => Ok(()),
        _ => Err(A2AClientError::InvalidUrl(url.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_http_and_https() {
        assert!(validate_url("http://localhost:8000").is_ok());
        assert!(validate_url("https://agent.example.com/").is_ok());
    }

    #[test]
    fn validate_url_rejects_bad_scheme() {
        assert!(validate_url("ftp://host").is_err());
        assert!(validate_url("not-a-url").is_err());
        assert!(validate_url("http://").is_err());
    }

    #[test]
    fn build_params_omits_context_when_none() {
        let p = build_message_params(json!({"role": "user"}), None, None);
        assert!(p.get("contextId").is_none());
        assert_eq!(p["metadata"], json!({}));
    }

    #[test]
    fn build_params_includes_context_and_metadata() {
        let p = build_message_params(
            json!({"role": "user"}),
            Some(json!({"skillId": "x"})),
            Some("ctx-1".into()),
        );
        assert_eq!(p["contextId"], json!("ctx-1"));
        assert_eq!(p["metadata"]["skillId"], json!("x"));
    }

    #[test]
    fn terminal_event_detection() {
        let completed = json!({"statusUpdate": {"status": {"state": "TASK_STATE_COMPLETED"}}});
        let working = json!({"statusUpdate": {"status": {"state": "TASK_STATE_WORKING"}}});
        assert!(is_terminal_event(&completed));
        assert!(!is_terminal_event(&working));
        assert!(!is_terminal_event(&json!({"artifactUpdate": {}})));
    }

    #[test]
    fn jsonrpc_error_maps_to_typed() {
        assert!(matches!(
            A2AClientError::from_jsonrpc(-32001, "x"),
            A2AClientError::TaskNotFound { .. }
        ));
        assert!(matches!(
            A2AClientError::from_jsonrpc(-32002, "x"),
            A2AClientError::TaskNotCancelable { .. }
        ));
        assert!(matches!(
            A2AClientError::from_jsonrpc(-32603, "boom"),
            A2AClientError::Server { code: -32603, .. }
        ));
    }
}
