# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-08-17

Minor release. Task isolation moves into the `TaskStore`, which makes it survive
a restart — a **breaking** change to the store traits and to
`A2AServerFactory::create`; see *Changed (BREAKING)* below for the migration
table. Also carries conformance and correctness fixes on the A2A server path
from `aiperceivable/apexe` issues #33, #34 and #35, and raises the apcore floor
to 0.27. Deployments on the default in-memory stores and on `build_app` /
`serve` need no migration. 161 tests pass.

### Fixed

- **Failed tasks no longer collapse every error to `"Internal server error"`.**
  `error_to_status` now routes through `ErrorMapper`, the crate's single
  redaction policy, so the task-status surface classifies like the JSON-RPC
  surface. Internal and unrecognized errors keep the fixed string (srs
  FR-ERR-004 / FR-ERR-008) and ACL denials stay masked (FR-ERR-003), but
  caller-fixable failures — schema validation, invalid input, unknown module —
  carry their sanitized detail plus `ai_guidance` when apcore supplied one. An
  agent that reads a guard refusal can now correct itself. Python and TS emit
  the fixed string on this path too, so they need the same change.

  `ai_guidance` is gated on exactly those three classes, not on
  `err.user_fixable`. Six apcore codes carry `user_fixable = Some(true)` while
  mapping to the fixed "Internal server error" (`VERSION_CONSTRAINT_INVALID`,
  the three `BINDING_*` codes, `DEPENDENCY_NOT_FOUND`,
  `DEPENDENCY_VERSION_MISMATCH`), and `user_fixable` is settable per-error by
  the module author — so the first version of this change let a fixed,
  deliberately-opaque string be extended with internal detail that
  `sanitize_message` does not strip (module ids, versions, env-var names,
  hostnames). A unit test now locks the gate to `ErrorMapper`'s own branching
  across every apcore error code.

  `SCHEMA_VALIDATION_ERROR` is no longer treated as caller-fixable in every
  direction. apcore raises the one code for input, **output** and config
  validation, so a module returning the wrong shape reached the caller as
  `-32602 Invalid params` with `"Output validation failed"` and guidance
  pointing at a `details.errors` field an A2A caller never receives — a
  server-side defect reported as the caller's fault. Output and config
  validation now map to the fixed internal string. The direction label apcore
  puts at the front of the message is the only signal available, so the two
  exact wordings are matched; anything unrecognized (including a module raising
  the code with its own wording) keeps the caller-facing detail.
- **`tasks/cancel` is guarded.** Unknown ids return `-32001` instead of
  fabricating a CANCELED task, terminal tasks return `-32002` instead of having
  their artifacts destroyed, and a cancelled task keeps the artifacts and
  history it had already accumulated (srs FR-TSK-005; matches a2a-python's
  `on_cancel_task`).
- **`TextPart` input works.** The module's input schema is now passed to the
  part converter, so a JSON text part is parsed against it rather than arriving
  as a bare string — making the `application/json` input mode on the Agent Card
  usable without a `DataPart`.
- **JSON-RPC 2.0 envelope validation.** Malformed JSON returns `-32700` in a
  JSON-RPC response instead of a `text/plain` HTTP 400; a `"jsonrpc"` other
  than `"2.0"`, a missing `jsonrpc`/`method`, and a batch array all return
  `-32600` (previously accepted, or reported as `-32601`).

  A missing `id` is deliberately *not* an error. That is a JSON-RPC 2.0
  notification, and — like a2a-python and a2a-js — this server answers it with
  a normal response carrying `"id": null` rather than staying silent as strict
  JSON-RPC 2.0 would. Clients that send notifications should expect a response
  body.
- **SSE frames are JSON-RPC responses.** Each `data:` now carries
  `{"jsonrpc","id","result":<event>}`, matching a2a-python and a2a-js, so an
  off-the-shelf A2A client can parse the stream. Event ordering, the terminal
  `lastChunk` marker and the `oneof` wrapper keys are unchanged; `kind` and
  `final` remain absent, as A2A 1.0 requires.

  The SSE `id:` line also remains absent, but that is a **deviation from the
  spec repo**, which mandates a monotonic `id:` in three places
  (`docs/spec/srs.md`, `docs/features/streaming.md`,
  `docs/spec/tech-design.md`). Neither a2a-python nor a2a-js emits one, so
  emitting it would put this server alone on the wire, and it buys nothing
  until `tasks/resubscribe` / `Last-Event-ID` replay exists. To be revisited
  with resubscribe support, or by amending the spec.
- **`"role": "user"` is accepted.** The lowercase A2A 0.3 spellings are
  deserialization aliases (`ROLE_*` is still what is serialized), and an
  unreadable `message` now reports what actually failed to parse instead of
  claiming the parameter was missing.
- **`VERSION` tracks the crate version** (`CARGO_PKG_VERSION`) rather than a
  hand-maintained literal that had drifted to `0.4.1`.
- **A store-backend failure is reported as `-32603`, never as `-32001`.** The
  two demand opposite responses — an A2A agent reading *task not found*
  re-submits work that is actually still there, while *internal error* is
  correctly not caller-fixable — so they must stay distinguishable. This is the
  same classification the task-status surface uses (srs FR-ERR-004 /
  FR-ERR-008), now applied to every store-addressed method: `tasks/get`,
  `tasks/cancel`, `tasks/list`, and all three `tasks/pushNotificationConfig/*`.

  Three of those previously answered as though nothing was wrong:
  `tasks/pushNotificationConfig/delete` returned `result: null` while the
  config stayed live and kept delivering — a caller revoking a leaked webhook
  was told it had worked; `tasks/list` returned `[]`, which reads as "you have
  no tasks" rather than "the backend is down"; and
  `tasks/pushNotificationConfig/get` returned `-32001 not found` for an outage
  while `set` returned `-32603` for the same one, giving callers two
  contradictory signals about a single failure.

  `tasks/cancel` is the fourth: recording CANCELED *is* what cancelling does,
  so a failed write there is the operation failing, not "work happened that
  could not be recorded". It answered with a CANCELED task while the store
  still held the old state, leaving `tasks/get` to contradict it.

- **Task-store write failures are logged instead of discarded.** Each
  persistence call was a bare `let _ =`, so a store outage produced tasks that
  `message/send` reported as COMPLETED and `tasks/get` then reported as
  missing, with nothing logged in between. The response is unchanged — the work
  really did run — but the failure now reaches the operator.

### Changed (BREAKING)

- **`TaskStore` carries the owner; `PushConfigStore` is new.** Every
  `TaskStore` method now takes a `&CallContext` and returns `StoreError`
  instead of `String`, and `list` takes a `&ListParams`. Push-notification
  configs moved out of the server's own state into a matching
  `PushConfigStore` trait.

  This closes two holes that a process-memory `task_id -> owner` map beside the
  store could not: the map had to be capped (100 000 entries), which made an
  evicted task permanently unreachable to its owner; and it started empty after
  a restart, so **every task in a consumer-supplied persistent store was
  permanently unreachable to its genuine owner** — `tasks/get` / `tasks/cancel`
  and the three push-config methods returned `-32001`, `tasks/list` returned
  `[]`. Splitting `PushConfigStore` out is what stops a restart from restoring
  tasks whose webhook targets have evaporated. This is the shape upstream
  already uses (`a2a-python`'s `ServerCallContext` + `OwnerResolver`, and its
  separate `PushNotificationConfigStore`); `apcore-a2a-python` and
  `apcore-a2a-typescript` re-export those stores directly and were never
  affected.

  The default `InMemoryTaskStore` buckets by owner, so isolation is a property
  of the data structure rather than a check each method has to remember.

  | Change | Who is affected | Migration |
  |---|---|---|
  | `TaskStore` methods take `ctx: &CallContext` | custom `TaskStore` implementors | add the parameter; key storage on `(owner, task_id)` |
  | `Result<_, String>` -> `Result<_, StoreError>` | same | use `StoreError::backend` / `backend_msg` |
  | `list()` -> `list(&ListParams, &CallContext)` | same | add both parameters |
  | new `PushConfigStore` | callers of `A2AServerFactory::create` | pass `Arc::new(InMemoryPushConfigStore::new())`, or a persistent store alongside a persistent `TaskStore` |
  | `A2AServerFactory::create(...)` -> `create(registry, CreateOptions)` | same | build `CreateOptions::new(..)` and chain `with_task_store` / `with_auth` / `with_explorer` |
  | `AppState` fields are `pub(crate)` | anyone hand-building `AppState` | use `build_app` / `build_app_with_auth` / `serve` / `async_serve`, or `A2AServerFactory::create` |

  `build_app` / `build_app_with_auth` / `serve` / `async_serve` are unchanged;
  deployments on the default in-memory stores need no migration.

- **`AppState` is no longer constructible outside this crate**, and
  `A2AServerFactory::create` takes a `CreateOptions` struct instead of ten
  positional arguments. Three releases in a row had broken hand-built
  `AppState`s and `create` signatures; both surfaces are now closed, so adding
  a component later is not a breaking change. `CreateOptions` is also the only
  way to supply a custom store.

### Added

- **`storage` types are re-exported at the crate root** — `TaskStore`,
  `InMemoryTaskStore`, `PushConfigStore`, `InMemoryPushConfigStore`,
  `CallContext`, `OwnerId`, `ListParams`, `StoreError`. The documented
  `use apcore_a2a::InMemoryTaskStore;` did not compile before this.

### Changed

- **Requires apcore 0.27 (`>=0.27, <0.28`).** The floor was an unbounded
  `>=0.26`, so cargo was free to resolve a future major-breaking apcore into
  this crate. One 0.27 breaking change reaches the Agent Card:
  `Registry::describe` returns `Result<String, ModuleError>` instead of
  `String`, and its `Ok` value is now the cross-SDK Markdown *document* (an
  `# {module_id}` heading, `**Tags:**`, a `**Parameters:**` list,
  `**Documentation:**`) where through 0.26 it was the module's one-line
  `description()`. `AgentCardBuilder::build` fed that value straight into every
  `AgentSkill.description`, so on 0.27 the whole document would have been
  published on the card. It now reads `ModuleDescriptor::description`, which is
  the field `apcore-a2a-python`'s builder has always used
  (`agent_card.py:147`) and is the same string 0.26's `describe()` returned —
  so the card is byte-identical across the bump. Pinned by
  `skill_description_is_the_one_line_description_not_the_describe_document`,
  which asserts against the live `describe()` output so it cannot pass
  vacuously.

  No other 0.27 breaking change applies: this crate names none of
  `ErrorCode::ConfigurationError`, `RedactionConfig`, `with_coerce_types`,
  `_config.strict`, `pipeline.configure`, `inject_checked` or `StepMiddleware`,
  registers no step middleware, and asserts on no redacted output.
- **Source-breaking changes to the `server::handlers` surface.** The documented
  entry points (`build_app`, `build_app_with_auth`, `serve`, `async_serve`,
  `APCoreA2AConfig`, `BackendSource`) are unchanged; only code that constructs
  an `AppState` by hand or names the handler functions is affected.

  - `AppState` gained two required fields, `input_schemas` and `task_owners`,
    so struct-literal construction no longer compiles.
  - `AppState::agent_card` and `AppState::explorer_card` change from
    `Arc<Value>` to `Arc<FilteredCard>` (`.unfiltered()` returns the original
    `Arc<Value>`, `.for_caller(...)` the ACL-filtered copy).
  - `AppState::task_owners` is `Arc<Mutex<TaskOwners>>`, not
    `Arc<Mutex<HashMap<String, String>>>`.
  - `explorer_card` gained an `AuthIdentity` extractor argument (it must know
    the caller to filter the card). As an axum handler it is still routed the
    same way; only a direct call has to change.

### Security

- **All six task-addressed methods are scoped to the authenticated principal**
  — `tasks/list` / `tasks/get` / `tasks/cancel` and
  `tasks/pushNotificationConfig/set|get|delete`. `tasks/list` previously
  returned every caller's tasks including their output; a task could be read or
  cancelled by id from any caller; and the push-config methods checked nothing
  at all, so a principal holding another's task id could redirect that task's
  terminal `statusUpdate` to a webhook of its choosing, or silently suppress the
  owner's notifications by deleting their config. Only the unguessability of a
  UUIDv4 task id stood in the way. Cross-principal access is masked as `-32001`
  so task ids cannot be probed. The push-config methods now also require the
  task to exist, matching `a2a-python`'s `on_set/get_task_push_notification_config`.

  Ownership lives inside the store, keyed alongside the task itself, so it
  survives whatever the store survives — see the breaking `TaskStore` change
  below.

  Callers with no `Identity` share a single `""` owner bucket, as upstream's
  `UnauthenticatedUser` does — that covers both "no authenticator configured"
  and "an authenticator configured with `require_auth = false` that did not
  authenticate this request". Single-tenant deployments are unaffected; a
  permissive-mode deployment gets scoping only between authenticated callers,
  with every unauthenticated caller sharing one bucket.

  **Known limitation.** A consumer-supplied store that ignores its
  `CallContext` disables task isolation entirely: every caller sees every task.
  Rust cannot enforce the contract, and neither can upstream — `a2a-python`
  states the same requirement as a SHOULD on its own `TaskStore`. The contract
  is documented on the trait; if you supply a custom store, keeping it is
  yours.
- **The Agent Card advertises only ACL-allowed skills, and the filter agrees
  with enforcement.** The ACL gated the call but not the advertisement, so a
  deny-all-but-one ACL still disclosed the whole module inventory. The filter
  now evaluates each skill exactly as apcore's pipeline does — against
  `acl_context(identity).child(skill_id)`, using that context's own `caller_id`
  — instead of against the authenticated principal with a `None` context. The
  first version of this filter got both halves wrong in opposite directions: a
  `callers: ["@external"] … deny` rule matched on the call path but not on the
  card, so an authenticated caller was advertised the entire inventory and then
  refused every call; and a rule carrying a `conditions:` block was silently
  inert on the card path (apcore's `check_conditions` returns false without a
  context) while it stayed live on the call path.

  **Writing caller rules: `callers:` names the calling module, not the
  principal.** An inbound A2A request is a top-level call, so it reaches
  `BuiltinACLCheck` with `caller_id = None` and `ACL::check` evaluates it as
  `@external` — on the card path and the call path alike. That is apcore's
  contract, not a gap in it: `caller_id` is the calling module in a nested call
  chain, managed exclusively by `Context::child` (apcore `Context::create`
  doc — "top-level Contexts always have `caller_id = None`"). It is also the
  behaviour operators depend on, since `callers: ["@external"] … deny` is the
  natural way to lock external traffic out, and it has to keep matching an
  authenticated request or it silently stops covering what it was written for.

  The designed way to discriminate principals is `@system` (matched against
  `identity.identity_type`) or an `identity_types` / `roles` condition. Both
  read `ctx.identity`, which `Context::child` clones through unchanged and
  `BuiltinACLCheck` passes to `ACL::check`, so both work today on both
  surfaces. A rule naming a principal in `callers:` — `callers: ["u1"]` — will
  never match, and the card is careful not to pretend otherwise.

  One real consequence remains: the audit trail's caller dimension, the circuit
  breaker's per-caller key and the obs/otel caller attribute read `caller_id`
  rather than `identity`, so they record `@external` for every inbound request.

  The filtered card is memoized per caller. `ACL::check` invokes the consumer's
  audit sink — a synchronous `Fn(&AuditEntry)` — once per skill, and the
  discovery path is auth-exempt, so filtering on every request let any
  anonymous client emit `skills.len()` governance entries per request at
  arbitrary rate (each recording `decision: "deny"`, indistinguishable from a
  real enforcement decision) while blocking a tokio worker for as long as the
  sink took. Memoizing is sound because an installed `ACL` cannot change for
  the life of the process. The sink is still driven once per (caller, card):
  apcore's public `ACL` API offers no way to suppress it — `default_effect` is
  private, so an audit-free twin cannot be rebuilt from `rules()`, and there is
  no `clear_audit_logger`.

## [0.4.4] - 2026-07-14

Patch release. Bumps the required `apcore` floor to `0.26` to align the ecosystem on the 0.26.0 governance layer (additive, no breaking changes). No code or API changes.

## [0.4.3] - 2026-07-07
update package dependency version for apcore-toolkit (0.10.0) and increment project patch version

## [0.4.2] - 2026-06-25

Patch release. Bumps apcore to 0.25.0 and apcore-toolkit to 0.9.1. No code or API changes; all 106 tests pass unmodified against the new runtime.

### Changed

- Dependency bump: `apcore = "0.25"` (from `"0.24"`) and `apcore-toolkit = "0.9.1"` (from `"0.8.1"`). The adapter's public surface is unaffected by the 0.24 → 0.25 delta.

  apcore 0.25.0 and apcore-toolkit 0.9.0–0.9.1 changes reviewed for adapter impact — none required a change:
  - **Config-driven ACL discovery (0.25.0, apcore #74)** — auto-wired during `APCore` construction, but skipped when the caller supplies its own `Executor` (as the adapter does); an explicitly configured ACL is never clobbered. No behavior change for the adapter.
  - **Registry module-id constants promoted to the public surface (0.25.0, apcore #30)** — export-surface-only addition; no behavior change.
  - **apcore-toolkit OpenAPI parser hardening (0.9.0–0.9.1)** — robustness fixes with no public API change; the adapter uses only `deep_resolve_refs`, which is unaffected.


## [0.4.1] - 2026-06-15

Patch release. Bumps apcore to 0.24.0 and apcore-toolkit to 0.8.1. No code or API changes; all 106 tests pass unmodified against the new runtime.

### Changed

- Dependency bump: `apcore = "0.24"` (from `"0.22"`) and `apcore-toolkit = "0.8.1"` (from `"0.8"`). The previous `apcore = "0.22"` caret requirement hard-capped below 0.23, so this bump was required to build against the 0.24 line. The adapter's public surface is unaffected by the 0.22 → 0.24 delta.

  apcore 0.23.0–0.24.0 changes reviewed for adapter impact — none required a change:
  - **Per-instance `ToggleState` (0.24.0, apcore #71)** — `Executor::new(registry, config)` is unchanged (the new per-instance `toggle_state` lives in `SysModulesOptions`, consumed by `register_sys_modules_with_options`); the adapter's `register_sys_modules(reg, &executor, &config, None)` 4-argument call remains valid and falls back to the process-global toggle state.
  - **`CircuitBreakerMiddleware` constructor rewrite (0.23.0, breaking)** — not used by the adapter; only the `ApcoreErrorCode::CircuitBreakerOpen` error code is mapped, which is unchanged.
  - **AI error-recovery metadata auto-populated on `ModuleError` (0.23.0)** and **`A2ASubscriber` 4xx no-retry (0.23.0)** — no adapter impact.


## [0.4.0] - 2026-06-01

Initial release — A2A 1.0 protocol adapter for apcore (Rust), at full feature
parity with the Python and TypeScript adapters. Built on axum 0.8 over apcore
0.22 + apcore-toolkit 0.8, with hand-rolled A2A 1.0 wire types (there is no Rust
A2A SDK).

### Added

- **A2A 1.0 server** (`serve` / `async_serve` / `build_app`): JSON-RPC dispatch —
  `message/send`, `message/stream` (SSE), `tasks/get`, `tasks/cancel`,
  `tasks/list`, `tasks/pushNotificationConfig/set|get|delete`. Agent Card served
  at `/.well-known/agent-card.json` (+ `/.well-known/agent.json` 0.3 alias) and
  `/health`.
- **A2A 1.0 wire types** (`src/types.rs`): `Part` flattened `oneof`
  (`{text}`/`{data}`/`{url}`/`{raw}`); `TaskState` / `Role` enums serializing
  full names (`TASK_STATE_*` / `ROLE_*`); events as the `oneof`
  `{task|statusUpdate|artifactUpdate}` (no `type`/`kind`/`final`); `AgentCard`
  with `supportedInterfaces`, `capabilities.{extensions,extendedAgentCard}`,
  `securityRequirements`, `signatures`.
- **Execution**: real streaming via `Executor::stream` (per-chunk
  `artifactUpdate` events); cooperative cancellation via per-task `CancelToken`;
  `global_deadline` mapped from `execution_timeout` (bounds the streaming path
  too).
- **Adapters**: `AgentCardBuilder`, `SkillMapper` (display overlay §5.13),
  `SchemaConverter` (`$ref` resolution via apcore-toolkit `deep_resolve_refs`),
  `ErrorMapper` + `A2aErrorFormatter`/`register_a2a_error_formatter` (§8.8),
  `PartConverter`.
- **Auth**: `JWTAuthenticator` with configurable `ClaimMapping` + tower
  middleware (`AuthMiddlewareLayer`) wired into the router — identity flows into
  the apcore `Context`; discovery/health exempt.
- **Ops**: `ObsLoggingMiddleware` on by default; `sys_modules` flag →
  `register_sys_modules`; CORS via `cors_origins`; Explorer UI at `/explorer`
  (+ `/explorer/agent-card` with per-skill `_inputSchemas`); webhook push
  delivery (3 retries, exponential backoff). Config Bus namespace `apcore-a2a`
  (env prefix `APCORE_A2A`, §9.13).
- **Client / storage / CLI**: `A2AClient` + `AgentCardFetcher`; `TaskStore`
  trait + `InMemoryTaskStore`; `apcore-a2a` binary; `APCoreA2A` builder +
  `APCoreA2AConfig`; `BackendSource` (`ExtensionsDir` / `Registry` / `Executor`).
- **Error mapping** (apcore `ModuleError` → A2A JSON-RPC): `MODULE_NOT_FOUND` →
  -32601; `SCHEMA_VALIDATION_ERROR` / `GENERAL_INVALID_INPUT` → -32602;
  `ACL_DENIED` → -32001 (masked "Task not found"); `MODULE_TIMEOUT` /
  `CALL_DEPTH_EXCEEDED` / `CIRCULAR_CALL` / `CALL_FREQUENCY_EXCEEDED` /
  `MODULE_DISABLED` / `CONFIG_*` → -32603.
- **Cross-language parity** (verified by the shared conformance suite): a JSON 401
  body `{error, detail}` with `content-type: application/json` + `WWW-Authenticate:
  Bearer`; `extended_agent_card` derived from authenticator presence (not from
  `security_schemes`); a missing `metadata.skillId` (or unconvertible parts) yields
  a **FAILED task** (not a JSON-RPC error); `securitySchemes` served in the proto3
  `oneof` shape; compact-JSON part serialization byte-identical to Python/TS.
- **Conformance suite** (`tests/conformance.rs`) mirroring the shared fixtures, and
  an Apache-2.0 **`LICENSE`**.
- 106 tests (unit + conformance + HTTP integration via `tower::oneshot`,
  including a live webhook-delivery test).

### Dependencies

- `apcore` 0.22, `apcore-toolkit` 0.8, `axum` 0.8, `tokio` 1 (full),
  `serde` / `serde_json` 1, `jsonwebtoken` 9, `reqwest` 0.12, `clap` 4,
  `thiserror` 2. Rust edition 2021.
