# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
