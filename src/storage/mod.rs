//! Task storage abstraction for A2A tasks.
//!
//! Provides the [`TaskStore`] and [`PushConfigStore`] traits, their in-memory
//! implementations, and the [`CallContext`] that scopes every stored item to
//! the principal that owns it.
//!
//! Ownership lives *inside* the store, not beside it. An earlier design kept a
//! separate process-memory `task_id -> owner` map in the server state, which
//! left two holes a consumer-supplied store could not close: the map had to be
//! capped (evicted tasks became unreachable to their owner), and it started
//! empty after a restart, so every task in a persistent store was unreachable
//! to its genuine owner for the rest of that process's life. Both disappear
//! once the store itself is owner-aware. This mirrors upstream `a2a-python`,
//! whose `TaskStore` methods all take a `ServerCallContext` and whose
//! `InMemoryTaskStore` buckets tasks by a resolved owner.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

use apcore::context::Identity;

/// The principal a stored task or push config belongs to.
///
/// Every request without an [`Identity`] resolves to the same empty owner,
/// exactly as upstream's `UnauthenticatedUser.user_name` does. That covers both
/// "no authenticator configured" — a single-tenant deployment keeps its current
/// behaviour — and an authenticator running with `require_auth = false`, whose
/// unauthenticated requests all share the one bucket while authenticated ones
/// are scoped normally.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OwnerId(String);

impl OwnerId {
    /// The shared bucket for requests carrying no authenticated identity.
    pub fn anonymous() -> Self {
        Self(String::new())
    }

    /// The owner for a request's authenticated principal.
    pub fn from_identity(identity: Option<&Identity>) -> Self {
        Self(identity.map(|i| i.id().to_string()).unwrap_or_default())
    }

    /// The owner as a string; empty for the anonymous bucket.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the shared anonymous bucket.
    pub fn is_anonymous(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for OwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-request context handed to every [`TaskStore`] and [`PushConfigStore`]
/// method.
///
/// The fields are private on purpose: adding a tenant, the full identity, or an
/// arbitrary state bag later is then a non-breaking change. Upstream's
/// equivalent (`a2a-python`'s `ServerCallContext`) grew exactly those fields
/// over time.
#[derive(Clone, Debug, Default)]
pub struct CallContext {
    owner: OwnerId,
}

impl CallContext {
    /// A context for an explicit owner.
    pub fn new(owner: OwnerId) -> Self {
        Self { owner }
    }

    /// A context for the shared anonymous bucket.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A context for a request's authenticated principal.
    pub fn from_identity(identity: Option<&Identity>) -> Self {
        Self::new(OwnerId::from_identity(identity))
    }

    /// The principal this call is scoped to.
    pub fn owner(&self) -> &OwnerId {
        &self.owner
    }
}

/// Filtering and pagination options for [`TaskStore::list`].
///
/// Empty today. It exists so options can be added without changing the trait
/// signature: `#[non_exhaustive]` stops callers outside this crate from
/// constructing it with a struct literal, so new fields stay non-breaking.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ListParams {}

impl ListParams {
    /// Default options: every task belonging to the caller.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Why a store operation failed — shared by [`TaskStore`] and
/// [`PushConfigStore`].
///
/// Distinguishes a backend failure from a poisoned in-process lock so a caller
/// can tell "the database is down" from "a previous panic corrupted this
/// store". Note that *not found* and *belongs to another owner* are not errors:
/// both surface as `Ok(None)` from [`TaskStore::get`], which is what makes task
/// ids unprobeable.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The backing store failed — I/O, database, serialization.
    #[error("task store backend failure: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A `Mutex` guarding in-process state was poisoned by a panic.
    #[error("task store lock poisoned")]
    LockPoisoned,
}

impl StoreError {
    /// Wrap a backend-specific error.
    pub fn backend<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(err))
    }

    /// Wrap a backend failure that has only a message, no error value.
    pub fn backend_msg(msg: impl Into<String>) -> Self {
        Self::Backend(msg.into().into())
    }
}

/// Persistence for A2A tasks, scoped to the calling principal.
///
/// # Contract
///
/// Every method **must** be scoped by [`CallContext::owner`]:
///
/// - [`get`](TaskStore::get) and [`delete`](TaskStore::delete) on a task
///   belonging to a different owner **must** behave exactly as they do for a
///   task that does not exist — `Ok(None)` and a no-op `Ok(())`. Never an
///   error, and never a distinguishable one: a caller must not be able to probe
///   for the existence of another principal's task id.
/// - [`list`](TaskStore::list) **must** return only `ctx.owner()`'s tasks.
/// - [`save`](TaskStore::save) **must** store under `ctx.owner()`. Two owners
///   may hold the same task id without seeing each other's copy.
///
/// The simplest way to satisfy all four at once is to bucket by owner — key the
/// backing map (or the database's primary key) on `(owner, task_id)` rather
/// than on `task_id` alone. Isolation is then a property of the data structure
/// instead of a check every method has to remember. [`InMemoryTaskStore`] does
/// this.
///
/// # This cannot be enforced
///
/// A store that ignores `ctx` compiles fine and disables task isolation
/// entirely: every caller sees every task. Rust's type system cannot prevent
/// it, and neither can upstream — `a2a-python` states the same requirement as a
/// SHOULD on its own `TaskStore`. If you supply a custom store, this contract
/// is yours to keep.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Store or replace `task_id` for `ctx.owner()`.
    async fn save(&self, task_id: &str, task: Value, ctx: &CallContext) -> Result<(), StoreError>;

    /// Read `task_id` if `ctx.owner()` owns it; `Ok(None)` if it does not exist
    /// *or* belongs to somebody else.
    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError>;

    /// Remove `task_id` if `ctx.owner()` owns it; a no-op otherwise.
    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError>;

    /// Every task belonging to `ctx.owner()`.
    async fn list(&self, params: &ListParams, ctx: &CallContext) -> Result<Vec<Value>, StoreError>;
}

/// Persistence for per-task push-notification webhook configs, scoped to the
/// calling principal.
///
/// Same contract as [`TaskStore`], and for the same reason: a principal holding
/// another's task id could otherwise point that task's terminal `statusUpdate`
/// at an attacker-controlled webhook, or silently suppress the owner's
/// notifications by deleting their config.
///
/// Kept separate from [`TaskStore`] so a persistent deployment can restore
/// tasks and their webhook configs together — a persistent `TaskStore` paired
/// with in-memory push configs would bring tasks back after a restart while
/// their delivery targets evaporated. Upstream splits them the same way
/// (`a2a-python`'s `PushNotificationConfigStore`).
#[async_trait]
pub trait PushConfigStore: Send + Sync {
    /// Store or replace the webhook config for `task_id` under `ctx.owner()`.
    async fn save(&self, task_id: &str, config: Value, ctx: &CallContext)
        -> Result<(), StoreError>;

    /// Read `task_id`'s webhook config if `ctx.owner()` owns it.
    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError>;

    /// Remove `task_id`'s webhook config if `ctx.owner()` owns it.
    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError>;
}

/// Owner-bucketed `owner -> task_id -> value` map behind one `Mutex`.
///
/// Shared by both in-memory stores: the isolation requirement is identical, so
/// implementing it twice would be two chances to get it wrong.
#[derive(Default)]
struct OwnerBuckets {
    buckets: Mutex<HashMap<OwnerId, HashMap<String, Value>>>,
}

impl OwnerBuckets {
    fn save(&self, key: &str, value: Value, ctx: &CallContext) -> Result<(), StoreError> {
        self.buckets
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .entry(ctx.owner().clone())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    fn get(&self, key: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError> {
        Ok(self
            .buckets
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(ctx.owner())
            .and_then(|bucket| bucket.get(key))
            .cloned())
    }

    fn delete(&self, key: &str, ctx: &CallContext) -> Result<(), StoreError> {
        let mut buckets = self.buckets.lock().map_err(|_| StoreError::LockPoisoned)?;
        if let Some(bucket) = buckets.get_mut(ctx.owner()) {
            bucket.remove(key);
            // Drop the owner entry once empty, so a stream of short-lived
            // anonymous owners cannot leak one empty map each.
            if bucket.is_empty() {
                buckets.remove(ctx.owner());
            }
        }
        Ok(())
    }

    fn list(&self, ctx: &CallContext) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .buckets
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(ctx.owner())
            .map(|bucket| bucket.values().cloned().collect())
            .unwrap_or_default())
    }
}

/// Default [`TaskStore`]: tasks bucketed by owner, held in process memory.
///
/// Contents do not survive a restart. A deployment that needs them to should
/// supply a store backed by a database — and keep the [`TaskStore`] contract
/// while doing so.
#[derive(Default)]
pub struct InMemoryTaskStore {
    tasks: OwnerBuckets,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn save(&self, task_id: &str, task: Value, ctx: &CallContext) -> Result<(), StoreError> {
        self.tasks.save(task_id, task, ctx)
    }

    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError> {
        self.tasks.get(task_id, ctx)
    }

    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError> {
        self.tasks.delete(task_id, ctx)
    }

    async fn list(
        &self,
        _params: &ListParams,
        ctx: &CallContext,
    ) -> Result<Vec<Value>, StoreError> {
        self.tasks.list(ctx)
    }
}

/// Default [`PushConfigStore`]: webhook configs bucketed by owner, held in
/// process memory.
#[derive(Default)]
pub struct InMemoryPushConfigStore {
    configs: OwnerBuckets,
}

impl InMemoryPushConfigStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PushConfigStore for InMemoryPushConfigStore {
    async fn save(
        &self,
        task_id: &str,
        config: Value,
        ctx: &CallContext,
    ) -> Result<(), StoreError> {
        self.configs.save(task_id, config, ctx)
    }

    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError> {
        self.configs.get(task_id, ctx)
    }

    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError> {
        self.configs.delete(task_id, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(owner: &str) -> CallContext {
        CallContext::new(OwnerId(owner.to_string()))
    }

    fn task(id: &str) -> Value {
        json!({ "id": id, "status": { "state": "completed" } })
    }

    #[tokio::test]
    async fn two_owners_may_hold_the_same_task_id_without_seeing_each_other() {
        // Bucketing, not a global id -> owner index: the same id in two buckets
        // is two independent records. The server never produces this (task ids
        // are server-generated UUIDv4s), but a store must not corrupt or leak
        // across owners if a consumer's ids ever collide.
        let store = InMemoryTaskStore::new();
        store
            .save("t1", json!({ "id": "t1", "who": "alice" }), &ctx("alice"))
            .await
            .unwrap();
        store
            .save("t1", json!({ "id": "t1", "who": "bob" }), &ctx("bob"))
            .await
            .unwrap();

        assert_eq!(
            store.get("t1", &ctx("alice")).await.unwrap().unwrap()["who"],
            "alice"
        );
        assert_eq!(
            store.get("t1", &ctx("bob")).await.unwrap().unwrap()["who"],
            "bob"
        );
    }

    #[tokio::test]
    async fn a_foreign_owner_cannot_tell_a_stored_task_from_a_missing_one() {
        // Both must be `Ok(None)`, never a distinguishable error, or task ids
        // become probeable.
        let store = InMemoryTaskStore::new();
        store.save("t1", task("t1"), &ctx("alice")).await.unwrap();

        assert!(store.get("t1", &ctx("mallory")).await.unwrap().is_none());
        assert!(store.get("nope", &ctx("mallory")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_only_the_callers_bucket() {
        let store = InMemoryTaskStore::new();
        store.save("a1", task("a1"), &ctx("alice")).await.unwrap();
        store.save("a2", task("a2"), &ctx("alice")).await.unwrap();
        store.save("b1", task("b1"), &ctx("bob")).await.unwrap();

        let mut alice: Vec<String> = store
            .list(&ListParams::new(), &ctx("alice"))
            .await
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect();
        alice.sort();
        assert_eq!(alice, vec!["a1", "a2"]);

        let bob = store.list(&ListParams::new(), &ctx("bob")).await.unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0]["id"], "b1");

        assert!(store
            .list(&ListParams::new(), &ctx("carol"))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn delete_by_a_foreign_owner_leaves_the_task_intact() {
        let store = InMemoryTaskStore::new();
        store.save("t1", task("t1"), &ctx("alice")).await.unwrap();

        // A no-op, not an error — same shape as deleting something absent.
        store.delete("t1", &ctx("mallory")).await.unwrap();
        assert!(store.get("t1", &ctx("alice")).await.unwrap().is_some());

        store.delete("t1", &ctx("alice")).await.unwrap();
        assert!(store.get("t1", &ctx("alice")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_anonymous_caller_shares_one_bucket() {
        // Matches upstream's `UnauthenticatedUser`: no authenticator, or an
        // authenticator with `require_auth = false`, means one shared owner.
        let store = InMemoryTaskStore::new();
        store
            .save("t1", task("t1"), &CallContext::anonymous())
            .await
            .unwrap();

        assert!(store
            .get("t1", &CallContext::from_identity(None))
            .await
            .unwrap()
            .is_some());
        assert!(OwnerId::anonymous().is_anonymous());
        assert_eq!(OwnerId::from_identity(None), OwnerId::anonymous());
    }

    #[tokio::test]
    async fn push_configs_are_scoped_to_their_owner_too() {
        let store = InMemoryPushConfigStore::new();
        let cfg = json!({ "url": "https://alice.example/hook" });
        store.save("t1", cfg.clone(), &ctx("alice")).await.unwrap();

        assert_eq!(store.get("t1", &ctx("alice")).await.unwrap().unwrap(), cfg);
        assert!(store.get("t1", &ctx("mallory")).await.unwrap().is_none());

        // A foreign delete must not suppress the owner's notifications.
        store.delete("t1", &ctx("mallory")).await.unwrap();
        assert!(store.get("t1", &ctx("alice")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn emptying_an_owners_bucket_drops_the_owner_entry() {
        let store = InMemoryTaskStore::new();
        store.save("t1", task("t1"), &ctx("alice")).await.unwrap();
        store.delete("t1", &ctx("alice")).await.unwrap();

        assert!(store.tasks.buckets.lock().unwrap().is_empty());
    }
}
