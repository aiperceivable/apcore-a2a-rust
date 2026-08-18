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
                        if let Ok(frame) = serde_json::from_str::<Value>(data) {
                            if let Some(err) = stream_frame_error(&frame) {
                                Err(err)?;
                            }
                            let event = unwrap_stream_envelope(frame);
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

    /// List tasks via `ListTasks`.
    ///
    /// A2A 1.0 names this method `ListTasks`; 0.3 had no task-listing method at
    /// all. `tasks/list` — used here until 0.5.0 — was neither, so it reached
    /// only this project's own Rust server.
    ///
    /// `limit` is kept as the friendly parameter name but goes on the wire as
    /// `pageSize`, which is what `ListTasksRequest` actually declares (alongside
    /// `pageToken`, `status`, `historyLength`, …). Sending `limit` earned an
    /// `-32602` from both SDK-backed servers.
    pub async fn list_tasks(&self, context_id: Option<String>, limit: i64) -> ClientResult<Value> {
        let mut params = json!({ "pageSize": limit });
        if let Some(cid) = context_id {
            params["contextId"] = Value::String(cid);
        }
        self.jsonrpc_call_versioned("ListTasks", params, Some("1.0"))
            .await
    }

    /// Close the client (releases the underlying HTTP connection pool).
    ///
    /// Provided for parity with Python/TS; the pool is also dropped when the
    /// client goes out of scope.
    pub async fn close(self) {}

    /// POST a JSON-RPC request, returning the `result` or a typed error.
    async fn jsonrpc_call(&self, method: &str, params: Value) -> ClientResult<Value> {
        self.jsonrpc_call_versioned(method, params, None).await
    }

    /// Send a JSON-RPC request, optionally declaring the A2A protocol version.
    ///
    /// Both upstream SDKs treat a request with no `A2A-Version` header as v0.3
    /// (spec section 3.6.2), and refuse 1.0 method names in that mode with
    /// `-32009`. Methods that only exist in 1.0 must therefore declare `"1.0"`;
    /// the ones 0.3 also has are left unversioned so 0.3 servers keep working.
    async fn jsonrpc_call_versioned(
        &self,
        method: &str,
        params: Value,
        a2a_version: Option<&str>,
    ) -> ClientResult<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": Uuid::new_v4().to_string(),
            "method": method,
            "params": params,
        });
        let url = format!("{}/", self.base_url);

        let mut request = self.http.post(&url).json(&body);
        if let Some(version) = a2a_version {
            request = request.header("A2A-Version", version);
        }
        let resp = request
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

/// Unwrap the JSON-RPC response envelope an SSE frame carries, yielding the
/// bare A2A stream event.
///
/// Frames without an envelope are passed through unchanged, so the client still
/// reads a server that emits bare payloads.
/// The client error a JSON-RPC error frame on the stream represents.
///
/// A mid-stream failure arrives as its own frame — upstream tags it
/// `event: error` and puts a JSON-RPC error response in `data:`. Envelope
/// unwrapping only looks for `result`, so without this the frame was yielded as
/// though it were an event and the failure was lost, while the non-streaming
/// path raised for byte-identical payload. Same `from_jsonrpc` mapping as there,
/// so a caller gets `TaskNotFound` / `TaskNotCancelable` on both paths.
fn stream_frame_error(frame: &Value) -> Option<A2AClientError> {
    frame.get("jsonrpc")?;
    let err = frame.get("error")?;
    let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603) as i32;
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(A2AClientError::from_jsonrpc(code, message))
}

fn unwrap_stream_envelope(frame: Value) -> Value {
    match (frame.get("jsonrpc"), frame.get("result")) {
        (Some(_), Some(result)) => result.clone(),
        _ => frame,
    }
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
    fn stream_error_frame_maps_to_the_same_error_as_a_unary_call() {
        // Upstream reports a mid-stream failure as its own frame (tagged
        // `event: error`). Envelope unwrapping only looks for `result`, so
        // without this check the frame was yielded as though it were an event
        // and the failure was lost — while the unary path errored on the same
        // payload. Both go through `from_jsonrpc`, so the variant matches.
        let frame = json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "error": { "code": -32001, "message": "Task not found" }
        });
        assert!(matches!(
            stream_frame_error(&frame),
            Some(A2AClientError::TaskNotFound { .. })
        ));
    }

    #[test]
    fn stream_error_frame_ignores_ordinary_events() {
        // A normal event carries `result`, never `error`.
        let event = json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "result": { "statusUpdate": { "status": { "state": "TASK_STATE_WORKING" } } }
        });
        assert!(stream_frame_error(&event).is_none());
        // And a bare event with no envelope at all is not an error either.
        assert!(stream_frame_error(&json!({ "statusUpdate": {} })).is_none());
    }

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
