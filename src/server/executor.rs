//! ApCoreAgentExecutor — bridges the apcore execution pipeline to A2A tasks.
//!
//! Wraps a real `apcore::executor::Executor` and exposes single-shot
//! (`call`) and streaming (`stream_channel`) execution. Each invocation runs
//! inside an apcore `Context` carrying a per-task `CancelToken` (A2A
//! `tasks/cancel`) and a `global_deadline` derived from `execution_timeout`
//! (bounds both the single-shot and streaming paths).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apcore::acl::ACL;
use apcore::cancel::CancelToken;
use apcore::context::{Context, Identity};
use apcore::errors::{ErrorCode, ModuleError};
use apcore::executor::Executor as ApcoreExecutor;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::adapters::parts::PartConverter;

/// Executes A2A tasks by delegating to an apcore [`ApcoreExecutor`].
pub struct ApCoreAgentExecutor {
    executor: Arc<ApcoreExecutor>,
    part_converter: PartConverter,
    execution_timeout_secs: u64,
}

impl ApCoreAgentExecutor {
    pub fn new(
        executor: Arc<ApcoreExecutor>,
        part_converter: PartConverter,
        execution_timeout_secs: u64,
    ) -> Self {
        Self {
            executor,
            part_converter,
            execution_timeout_secs,
        }
    }

    pub fn part_converter(&self) -> &PartConverter {
        &self.part_converter
    }

    /// The apcore ACL backing this executor, if one is configured. Lets the
    /// Agent Card advertise only the skills a caller is allowed to invoke.
    pub fn acl(&self) -> Option<Arc<ACL>> {
        self.executor.acl.clone()
    }

    /// Build an apcore `Context` carrying the cancel token and a global
    /// deadline (`now + execution_timeout`, fractional seconds since UNIX epoch
    /// per the apcore Rust contract).
    fn build_context(
        &self,
        identity: Option<Identity>,
        cancel_token: CancelToken,
    ) -> Context<Value> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let deadline = now + self.execution_timeout_secs as f64;
        Context::create(
            identity,
            None, // trace_parent
            Some(cancel_token),
            None,           // data
            Value::Null,    // services
            Some(deadline), // global_deadline
        )
    }

    /// Single-shot execution (`message/send`). Returns the raw apcore output.
    ///
    /// In addition to the cooperative `global_deadline` carried on the context,
    /// a hard host-side timeout (`execution_timeout_secs`) bounds the call so a
    /// non-cooperative module cannot block indefinitely. On elapse a
    /// `ModuleTimeout` error is returned (surfaced as a failed "Execution timed
    /// out" status, matching Python/TS).
    pub async fn call(
        &self,
        module_id: &str,
        inputs: Value,
        identity: Option<Identity>,
        cancel_token: CancelToken,
    ) -> Result<Value, ModuleError> {
        let ctx = self.build_context(identity, cancel_token);
        let fut = self.executor.call(module_id, inputs, Some(&ctx), None);
        match tokio::time::timeout(Duration::from_secs(self.execution_timeout_secs), fut).await {
            Ok(result) => result,
            Err(_) => Err(ModuleError::new(
                ErrorCode::ModuleTimeout,
                "Execution timed out",
            )),
        }
    }

    /// Streaming execution (`message/stream`). Spawns a task that drives the
    /// apcore stream and forwards each chunk result over a channel. Returns a
    /// `ReceiverStream` of raw apcore chunk results.
    ///
    /// A background task is used because `Executor::stream` borrows `&self`; by
    /// moving an `Arc<Executor>` clone into the task the returned stream is
    /// fully owned (`'static`).
    pub fn stream_channel(
        &self,
        module_id: String,
        inputs: Value,
        identity: Option<Identity>,
        cancel_token: CancelToken,
    ) -> ReceiverStream<Result<Value, ModuleError>> {
        let executor = self.executor.clone();
        let ctx = self.build_context(identity, cancel_token);
        let (tx, rx) = mpsc::channel::<Result<Value, ModuleError>>(16);
        let timeout = Duration::from_secs(self.execution_timeout_secs);

        tokio::spawn(async move {
            // A hard host-side timeout bounds the *entire* stream lifetime, in
            // addition to the cooperative `global_deadline`. (Limitation: this is
            // a total budget for the whole stream, not a per-chunk idle timeout;
            // a per-chunk timeout would require re-arming on each chunk.)
            let drive = async {
                let mut stream = executor.stream(&module_id, inputs, Some(&ctx), None);
                while let Some(item) = stream.next().await {
                    let is_err = item.is_err();
                    if tx.send(item).await.is_err() || is_err {
                        // Receiver dropped, or the stream errored (apcore ends the
                        // stream after an error). Stop forwarding.
                        break;
                    }
                }
            };
            if tokio::time::timeout(timeout, drive).await.is_err() {
                // Total budget elapsed: surface a timeout chunk so the handler
                // emits a failed "Execution timed out" status (Python/TS parity).
                let _ = tx
                    .send(Err(ModuleError::new(
                        ErrorCode::ModuleTimeout,
                        "Execution timed out",
                    )))
                    .await;
            }
        });

        ReceiverStream::new(rx)
    }
}
