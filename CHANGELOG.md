# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
- 37 tests (24 unit/conformance + 13 HTTP integration via `tower::oneshot`,
  including a live webhook-delivery test).

### Dependencies

- `apcore` 0.22, `apcore-toolkit` 0.8, `axum` 0.8, `tokio` 1 (full),
  `serde` / `serde_json` 1, `jsonwebtoken` 9, `reqwest` 0.12, `clap` 4,
  `thiserror` 2. Rust edition 2021.
