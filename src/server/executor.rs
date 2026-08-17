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

    /// Build an apcore `Context` carrying the authenticated identity, the
    /// cancel token and a global deadline (`now + execution_timeout`, fractional
    /// seconds since UNIX epoch per the apcore Rust contract).
    ///
    /// `caller_id` is deliberately left unset. It is **not** the authenticated
    /// principal: apcore defines it as the *calling module* in a nested call
    /// chain, managed exclusively by `Context::child` (apcore `context.rs`,
    /// `Context::create` doc — "top-level Contexts always have
    /// `caller_id = None`"). `BuiltinContextCreation` derives the child context
    /// for every call, and `Context::child` sets `caller_id` from
    /// `call_chain.last()`, which is empty on a top-level call — so an inbound
    /// A2A request reaches `BuiltinACLCheck` with `caller_id = None`, and
    /// `ACL::check` maps that to `@external`. That is the correct
    /// representation of "this call came from outside", not a limitation:
    /// `callers: ["@external"]` is how an operator denies external access, and
    /// it has to keep matching an authenticated request or the rule silently
    /// stops covering the traffic it was written for.
    ///
    /// The authenticated principal travels in `identity`, which `child()`
    /// clones through unchanged and `BuiltinACLCheck` passes to `ACL::check` as
    /// `Some(&ctx.context)`. Principal-based rules are therefore expressed with
    /// `@system` (matched against `identity.identity_type`) or with the
    /// `identity_types` / `roles` condition handlers — all of which do reach
    /// the ACL. See the same note in
    /// [`server::handlers::acl_filtered_card`](crate::server::handlers).
    ///
    /// What *is* a real consequence: the audit trail's caller dimension, the
    /// circuit breaker's per-caller key and the obs/otel caller attribute
    /// record `@external` for every inbound request, because they read
    /// `caller_id` rather than `identity`.
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
        Context::builder()
            .identity(identity)
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
    /// the two things apcore's `BuiltinContextCreation` does to a top-level
    /// context before `BuiltinACLCheck` reads it:
    ///
    /// 1. defaults a missing `caller_id` to `@external` and synthesizes a
    ///    matching external `Identity`;
    /// 2. replaces the context with `Context::child(module_id)`.
    ///
    /// Step 1 is applied here; step 2 is per-skill, so the caller applies
    /// `.child(skill_id)` to this base context and checks the ACL against the
    /// result — see `server::handlers::acl_filtered_card`. (Through apcore
    /// 0.26 step 2 lived in `BuiltinCallChainGuard`; 0.27 moved it into the
    /// non-removable `context_creation` step so the `testing` and `minimal`
    /// presets derive it too. The derivation itself is unchanged, so what this
    /// method reproduces is the same under both.)
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

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::config::Config;
    use apcore::registry::registry::Registry;

    use crate::adapters::schema::SchemaConverter;

    fn agent_executor() -> ApCoreAgentExecutor {
        let executor = ApcoreExecutor::new(Arc::new(Registry::new()), Config::default());
        ApCoreAgentExecutor::new(
            Arc::new(executor),
            PartConverter::new(SchemaConverter::new()),
            30,
        )
    }

    fn principal() -> Identity {
        Identity::new(
            "u1".to_string(),
            "test".to_string(),
            vec!["admin".to_string()],
            HashMap::new(),
        )
    }

    #[test]
    fn build_context_leaves_caller_id_unset_and_carries_the_principal_in_identity() {
        // apcore's contract: `caller_id` is the calling MODULE in a nested call
        // chain, managed exclusively by `Context::child` — "top-level Contexts
        // always have `caller_id = None`" (apcore `Context::create` doc). The
        // authenticated principal travels in `identity`, which is what the ACL
        // reads through `@system` and the `identity_types` / `roles` handlers.
        let ctx = agent_executor().build_context(Some(principal()), CancelToken::new());

        assert!(
            ctx.caller_id.is_none(),
            "a top-level context must not name a caller"
        );
        assert_eq!(ctx.identity.as_ref().map(Identity::id), Some("u1"));
        assert!(ctx.global_deadline.is_some());
    }

    #[test]
    fn acl_context_child_reaches_external_while_keeping_the_real_identity() {
        // What `BuiltinContextCreation` hands to `BuiltinACLCheck`, reproduced
        // for the Agent Card filter. Threading the principal into `caller_id`
        // would be inert — `child()` overwrites it from `call_chain.last()`,
        // which is empty at top level — so an authenticated request is checked
        // as `@external` on BOTH surfaces. That is what keeps a
        // `callers: ["@external"] … deny` rule covering authenticated traffic.
        let base = agent_executor().acl_context(Some(principal()));
        assert_eq!(base.caller_id.as_deref(), Some(EXTERNAL_CALLER));
        assert_eq!(base.identity.as_ref().map(Identity::id), Some("u1"));

        let ctx = base.child("test.echo");
        assert!(
            ctx.caller_id.is_none(),
            "child() re-derives caller_id from an empty call_chain"
        );
        assert_eq!(
            ctx.identity.as_ref().map(Identity::id),
            Some("u1"),
            "the principal must survive child() — it is how the ACL discriminates callers"
        );
        assert_eq!(
            ctx.identity.as_ref().map(Identity::identity_type),
            Some("test")
        );
    }

    #[test]
    fn acl_context_synthesizes_the_external_identity_for_an_anonymous_caller() {
        let ctx = agent_executor().acl_context(None);
        assert_eq!(ctx.caller_id.as_deref(), Some(EXTERNAL_CALLER));
        assert_eq!(
            ctx.identity.as_ref().map(Identity::id),
            Some(EXTERNAL_CALLER)
        );
        assert_eq!(
            ctx.identity.as_ref().map(Identity::identity_type),
            Some("external")
        );
    }
}
