//! ApCoreAgentExecutor — bridges the apcore execution pipeline to A2A tasks.
//!
//! Wraps a real `apcore::executor::Executor` and exposes single-shot
//! (`call`) and streaming (`stream_channel`) execution. Each invocation runs
//! inside an apcore `Context` carrying a per-task `CancelToken` (A2A
//! `tasks/cancel`) and a `global_deadline` derived from `execution_timeout`
//! (bounds both the single-shot and streaming paths).

use std::collections::HashMap;
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

/// apcore's sentinel for an unauthenticated caller. Mirrors
/// `apcore::sys_modules::DEFAULT_EXTERNAL_CALLER`, which is crate-private there.
const EXTERNAL_CALLER: &str = "@external";

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

    /// Build an apcore `Context` carrying the caller principal, the cancel
    /// token and a global deadline (`now + execution_timeout`, fractional
    /// seconds since UNIX epoch per the apcore Rust contract).
    ///
    /// `caller_id` is set from the authenticated identity (via the builder;
    /// `Context::create` hard-sets `caller_id: None`). An anonymous request
    /// leaves it unset so apcore applies its own `@external` default.
    ///
    /// **apcore 0.26 discards this value before any consumer reads it**, so
    /// today it changes nothing: the standard pipeline runs
    /// `BuiltinCallChainGuard` before `BuiltinACLCheck`, and that step replaces
    /// the whole context with `Context::child(module_id)`, which derives
    /// `caller_id` from `call_chain.last()` — empty on a top-level call, so
    /// `caller_id` becomes `None` again. Every inbound A2A request therefore
    /// reaches the ACL, the audit trail, the circuit-breaker key and the
    /// obs/otel caller attribute as `@external`, whoever sent it, and no host
    /// can change that from outside apcore. It is set here anyway because
    /// [`Self::acl_context`] reproduces the same `child()` derivation for the
    /// Agent Card filter: the two surfaces agree under today's behaviour, and
    /// would still agree — with the real principal — if apcore's `child()`
    /// learned to preserve an explicitly-set top-level `caller_id`.
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
        let caller_id = identity.as_ref().map(|id| id.id().to_string());
        Context::builder()
            .identity(identity)
            .caller_id(caller_id)
            .cancel_token(Some(cancel_token))
            .services(Value::Null)
            .global_deadline(Some(deadline))
            .build()
    }

    /// Build the base `Context` an out-of-pipeline ACL decision (the Agent Card
    /// filter) must be evaluated against.
    ///
    /// Discovery and enforcement have to reach the same verdict, or the card
    /// advertises skills every call refuses, or hides skills the caller can
    /// invoke. Rather than guess at the pipeline's behaviour, this reproduces
    /// the two things apcore's standard pipeline does to a top-level context
    /// before `BuiltinACLCheck` reads it:
    ///
    /// 1. `BuiltinContextCreation` defaults a missing `caller_id` to
    ///    `@external` and synthesizes a matching external `Identity`;
    /// 2. `BuiltinCallChainGuard` replaces the context with
    ///    `Context::child(module_id)`.
    ///
    /// Step 1 is applied here; step 2 is per-skill, so the caller applies
    /// `.child(skill_id)` to this base context and checks the ACL against the
    /// result — see `server::handlers::acl_filtered_card`.
    ///
    /// The context itself matters as much as the principal: passing `None`
    /// makes apcore's `check_conditions` return `false` unconditionally, so
    /// every rule carrying a `conditions:` block was inert on the card path
    /// while it stayed live on the call path, and `@system` (which reads
    /// `ctx.identity`) could never match.
    pub fn acl_context(&self, identity: Option<Identity>) -> Context<Value> {
        let mut ctx = self.build_context(identity, CancelToken::new());
        if ctx.caller_id.is_none() {
            ctx.caller_id = Some(EXTERNAL_CALLER.to_string());
            if ctx.identity.is_none() {
                ctx.identity = Some(Identity::new(
                    EXTERNAL_CALLER.to_string(),
                    "external".to_string(),
                    vec![],
                    HashMap::new(),
                ));
            }
        }
        ctx
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
