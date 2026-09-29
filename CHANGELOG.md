# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.1] - 2026-09-29

Adopts apcore-toolkit 0.13.0, whose `OpenAPIScanner` now emits every `module_id` in apcore's
Canonical ID alphabet itself, and retires this binding's own module-ID projection (FR-OAS-002)
to match. Tracks the `apcore-a2a` spec's `openapi_backend.json` contract 2.0.

Suite: 229 tests with `--all-features` (was 224), 196 without; `cargo fmt --check`,
`cargo clippy --all-targets -D warnings` (with and without `--all-features`) and
`cargo build --examples` clean.

### Changed — BREAKING

- **OpenAPI-derived module IDs — and therefore A2A skill IDs — change for every camelCase or
  hyphenated `operationId` and path.** The toolkit splits camelCase into snake_case words where
  this binding's projection only lowercased: `listPets` → `list_pets` (was `listpets`); under
  `prefix: petstore`, `petstore.list_pets` (was `petstore.listpets`); and camelCase path
  parameters likewise (`GET /pets/{petId}` → `pets.pet_id.get`, was `pets.petid.get`). A
  `prefix` is normalised with the rest of the ID (`Pet-Store` → `pet_store.…`). IDs that were
  already lowercase and legal are unchanged. **Migration:** ACL rules (`targets`), bindings, and
  `include` / `exclude` patterns keyed on the old IDs must be updated — the scanner's filters
  match the emitted, normalised ID (`^read_audit_log$`, not `readAuditLog`). The recommended
  prefixed catch-all deny rule (`petstore.*`) keeps holding, but an allow-list of old operation
  names now fails closed until it is updated. Clients that call skills by ID must use the new IDs.
- **Required `apcore-toolkit` floor raised to 0.13.0** (was `>=0.12.0`), keeping
  `default-features = false` and the `openapi` → `apcore-toolkit/http-proxy` wiring.
  `Cargo.lock` is not tracked; a local checkout needs `cargo update -p apcore-toolkit`.
- **FR-OAS-002 is now a skip policy, applied after `scan`.** `openapi_backend` no longer
  rewrites IDs: it registers the ID the scanner emitted. It still skips a module whose emitted ID
  apcore's registry would reject — the one case the toolkit deliberately does not repair, a
  segment that begins with a digit (`/v1/2fa` → `v1.2fa.get`), or an empty ID from a hook — with
  the same WARNING as before, now checked on the IDs `scan` returns instead of inside
  `transform_module`. A hook returning `MyThing` is normalised by the toolkit to `my_thing` and
  registers (the old in-hook projection made it `mything`; a legality check left there would have
  skipped it). The toolkit's own legality warning for a skipped module is not re-emitted beside
  the skip line. The caller's `transform_module` is now handed to the scanner as it is, so it
  still runs first — and the `Arc<Mutex<Vec<_>>>` side channels the wrapping hook needed are
  gone.
- **The FR-OAS-003 description repair runs on the modules `scan` returns**, still after the
  caller's `transform_module` hook, so the synthesis INFO line names the IDs actually emitted —
  including a deduplicated `…_2`, which the in-hook repair, running before the scanner's
  deduplication, reported under the pre-deduplication ID (`list_pets` twice, never
  `list_pets_2`).

### Deprecated

- **`project_module_id`** (`#[deprecated]`), at both `apcore_a2a::openapi_backend` and the
  crate root. apcore-toolkit >= 0.13 emits IDs in apcore's alphabet, so the projection is no
  longer needed and nothing in this crate calls it. Behaviour unchanged; it will be removed in a
  later minor release. It does not reproduce the toolkit's naming (`listPets` → `listpets`, not
  `list_pets`), so do not use it to predict the ID a module registers under. `is_legal_segment`
  is unaffected — the skip policy uses it.

### Fixed

- **An operation removed by `include` / `exclude` was still reported as description-synthesized.**
  The repair ran inside the scanner's `transform_module` hook, which runs before the scanner's
  own filters, so the FR-OAS-003 INFO line named operations that never registered and counted
  them against a denominator that excluded them (`2 of 1 scanned operations`). The feature spec
  already required the opposite ("an operation excluded by configuration is never synthesized for
  and never reported"); running the repair after `scan` makes it true.

### Tests

- The conformance driver reads `openapi_backend.json` contract 2.0: re-pinned IDs; three new
  cases (`projection_hook_output_normalised_not_skipped`,
  `description_synthesis_report_names_the_emitted_id`,
  `description_not_synthesized_for_excluded_operation`); the fixture's named `hooks` (an unknown
  hook name panics); `expected_no_error_logs`; and `expected_synthesis_report`.
- New regressions: production code never calls `project_module_id` (read from the source, outside
  its `#[cfg(test)]` module); an illegal hook-returned ID is skipped whatever its shape (including
  the empty ID); a skipped module reaches no later diagnostic (no synthesis, no FR-OAS-005 count,
  the zero-modules warning fires, nothing at ERROR); a caller hook's `Pet-Store.ListPets`
  registers as `pet_store.list_pets` with its cleared description repaired; `illegal_segment`
  reads the emitted ID as it is.

## [0.8.0] - 2026-09-24

Minor release, version-aligned with the Python and TypeScript SDKs. Raises the required floor to
`apcore` 0.31.0 and `apcore-toolkit` 0.12.0, and fixes a card/enforcement divergence the floor
raise would otherwise have reopened. 224 tests pass (was 224 — one test rewritten in place, no
new surface).

### Fixed

- **`ApCoreAgentExecutor::acl_context` no longer synthesizes an `Identity` for an anonymous
  caller** (`src/server/executor.rs`). This method exists to reproduce, out of pipeline, exactly
  what apcore's `BuiltinContextCreation` hands to `BuiltinACLCheck`, so the Agent Card filter and
  the real call path agree about what an anonymous caller can see. Through apcore 0.30, apcore-rust
  itself manufactured an `Identity{id:"@external", type:"external"}` for a null identity — a
  Rust-only bug apcore-python and apcore-typescript never had — and this method mirrored it for the
  same reason it mirrors everything else `BuiltinContextCreation` does. apcore 0.31.0 fixes
  apcore-rust to match its siblings (`PROTOCOL_SPEC` decision D-103: a null `identity` stays null,
  no synthetic `@external` principal; `caller_id` alone still defaults to the ACL's `@external`
  sentinel). Left unmirrored, this method would have kept manufacturing an `Identity` the real
  pipeline no longer does, silently reopening the exact discovery/enforcement disagreement this
  method was written to prevent: a `identity_types`/`roles` conditional ACL rule would evaluate
  against a fabricated principal on the card path and against `None` on the call path. `caller_id`
  defaulting is unaffected — only the `identity` synthesis was removed.

### Changed — dependency floor

- **Required `apcore` floor raised to 0.31.0** (was `>=0.30`) and **required `apcore-toolkit`
  floor raised to 0.12.0** (was `>=0.11.1`). apcore 0.31.0 is two joined audit cycles
  (`PROTOCOL_SPEC` v1.37.0 → v1.59.0) settling 54 cross-language divergences, five of them
  security defects; apcore-toolkit 0.12.0 adds the Device Authorization Flow (RFC 8628, unused
  here), threads a `pattern` parameter through `BindingLoader.load` (not used by this crate), and
  fixes a `$ref` sibling-key credential-disclosure defect in its own schema resolver. Every
  `apcore`/`apcore-toolkit` symbol this crate imports was grepped against both changelogs'
  breaking-change sections: the one hit is the `acl_context` fix above. `src/adapters/schema.rs`
  delegates `$ref` resolution entirely to `apcore_toolkit::deep_resolve_refs`, so the toolkit's
  sibling-key fix reaches this crate transitively with no code change needed here — unlike
  `apcore-mcp`, which carried an independent, unfixed copy of the same bug in its own schema
  converter. `sys_modules.enabled` registration, `global_deadline` construction (already a
  dedicated `Context` field, already epoch seconds, already built fresh per request rather than
  deserialized), and `governance_state()`/`check_access` usage were all checked against their
  respective decisions and need no change.

## [0.7.0] - 2026-09-07

Minor release, version-aligned with the Python and TypeScript SDKs. Ships the **OpenAPI
Backend** (feature F-12): point the adapter at an OpenAPI 3.0/3.1 document and every
operation becomes an A2A Skill, proxied over HTTP to the API that published it, with no
apcore project on the other end.

Suite: 224 tests with `--all-features` (was 190), 194 without — the 26 that need a spec
fetch or an HTTP proxy writer are behind the feature. `cargo clippy --all-targets
--all-features -- -D warnings` and `cargo fmt --all -- --check` clean.

### Added

- **`apcore_a2a::openapi_backend`**, behind the new `openapi` cargo feature —
  `openapi_backend()`, the async `openapi_backend_from_spec()` wrapper (apcore-toolkit-rust's
  `load_spec` is `async`, so the fetch cannot hide inside a document argument),
  `project_module_id`, `is_legal_segment`, `resolve_spec_location`, `synthesize_description`,
  `build_openapi_backend_from_config` and `OpenAPIBackendOptions`. All re-exported from the
  crate root.

  The pipeline is `load_spec -> OpenAPIScanner::scan -> HTTPProxyRegistryWriter::write ->
  Registry`, all already-shipped apcore-toolkit code; everything downstream is the adapter
  that already serves an extensions directory, unmodified. See
  `apcore-a2a/docs/features/openapi-backend.md`.

- **`BackendSource::OpenApi { spec, options }`.** It resolves to `(Executor, Some(Registry))`,
  not `(Executor, None)`: `apcore::register_sys_modules` takes an owned `Arc<Registry>` while
  `Executor::registry()` yields only a `&Registry`, so returning `None` would silently cost
  `system.*` registration on an OpenAPI-backed server — which is the whole reason the feature
  spec makes this source produce a `Registry` rather than an `Executor`. The registry is
  populated before the executor is constructed.

- **Two repairs the composition cannot work without.** `FR-OAS-002`: apcore-toolkit derives
  module IDs into `[A-Za-z0-9_.-]` while apcore's `Registry` accepts only
  `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$` — measured against apcore 0.30.0 / apcore-toolkit
  0.11.1, the canonical Swagger Petstore scans cleanly, registers **nothing**, and yields an
  Agent Card with zero skills without raising anywhere. A projected ID that still carries an
  unrepairable segment (`2fa` cannot begin with a digit) drops the module and says so at
  WARNING, naming both the derived ID and the segment — the scanner's `transform_module` hook
  drops a `None` return *silently*, so that report cannot be delegated. `FR-OAS-003`: an
  operation with neither `summary` nor `description` yields `""`, and `AgentCardBuilder` skips
  empty-description modules, so undocumented operations vanished from the card with no
  diagnostic; a `{METHOD} {path}` description is synthesized instead and the affected modules
  are named at INFO — by their **post**-projection IDs, which are the ones that reach the card.

  The ordering is normative in both directions: the caller's own `transform_module` hook runs
  first, then the description repair, then the projection — and the projection runs inside the
  scanner's hook, so it happens before the scanner's own `deduplicate_ids`, because lowercasing
  can *create* a collision the document did not have (`listPets` + `listpets`).

- **`FR-OAS-005` unapproved-write warning.** The scanner never infers `requires_approval` for
  any HTTP method, and the 0.6.0 public-card filter subtracts only ACL-denied and
  approval-gated skills — so a scanned `POST /charges` is advertised on the unauthenticated
  `/.well-known/agent-card.json`. The warning names that exposure, and is **never** suppressed
  by the presence of an ACL (apcore's own `GovernanceState` reports the absence of a gate,
  never the presence of protection), only by having nothing to warn about, by a module
  declaring `requires_approval` itself, or by an explicit `acknowledge_unapproved_writes`. It
  is escalated when `builtin_approval_gate_wired` is `false`.

- **CLI:** `--from-openapi`, `--openapi-base-url`, `--openapi-prefix`, `--openapi-include`,
  `--openapi-exclude`, `--openapi-header` (repeatable `KEY:VALUE`, spec fetch only) and
  `--openapi-no-deprecated`. Each is an overlay on the `apcore-a2a.openapi` Config Bus
  section, resolved per key. An extensions directory combined with an OpenAPI document
  requires a prefix from either route, and they populate one registry so the collision
  preflight can see the project modules it must not shadow.

- **Config:** an `apcore-a2a.openapi` default in the namespace registration — the namespace's
  first nested section and its first path-typed key. apcore 0.30.0's protections for
  path-typed keys do not reach a consumer namespace: `Config::path_typed_keys()` is a
  hardcoded set of apcore's own keys and never consults a namespace registered through
  `Config::register_namespace`, so `resolve_spec_location` owns the three rules instead.

- **`cli::merge_openapi_settings`**, which reads that section and overlays the `--openapi-*`
  flags on it **per key**, flag wins. It returns `None` when neither route names a `spec` —
  the ordinary "no OpenAPI configured" outcome, not an error — and the CLI's backend-source
  check consults it, so `apcore-a2a` with **no flag at all** now serves a spec declared in
  the config file.

- **New `openapi` cargo feature**, gating `apcore-toolkit/http-proxy` (the spec fetch and the
  HTTP proxy writer), matching the Python package's `openapi` extra. `apcore-toolkit` is now
  depended on with `default-features = false` so the toolkit's *default* `http-proxy` feature
  is reached through that gate rather than pulled in unconditionally.

- **9 conformance tests** (`tests/openapi_backend_conformance.rs`) against the shared corpus
  in `apcore-a2a/conformance/fixtures/openapi_backend.json`, plus 17 unit tests. The driver
  captures `tracing` output **with its level**, so every diagnostic the fixture names is
  asserted rather than assumed — the two discriminating cases
  (`projection_unprojectable_segment_dropped_with_warning`,
  `write_warning_not_suppressed_by_permissive_acl`) assert nothing *but* a diagnostic. Three
  behaviours the shared corpus cannot reach — `timeout` being the spec-**fetch** timeout,
  `headers` reaching the fetch request, and `base_url` supplying what a document lacks — are
  pinned separately, two of them against a real socket.

### Fixed (conformance harness)

- **The conformance runner could pass vacuously.** `cases()` read each fixture group through
  `unwrap_or_default()`, so renaming a group upstream made the loop iterate zero cases and
  report success. Measured by renaming `test_cases`: this binary reported 9 passing tests while
  asserting nothing, and apcore-a2a-typescript silently dropped from 46 to 33, while
  apcore-a2a-python failed loudly because it indexes the key directly. `cases()` now panics on
  an absent or empty group. Returning early when the spec repo is not checked out remains a
  separate, legitimate case.

- **The three CLIs disagreed on the exit code for a usage fault, and Rust disagreed with
  itself.** Measured before the fix: Python exited `2` for a usage fault and `1` for a
  configuration fault — correct; TypeScript collapsed everything onto `1`; and Rust exited `2`
  for `--bogus` (clap caught it) but `1` for a missing backend source (its own check did) — the
  same class of mistake, two codes, decided by which layer noticed first.

  All three now implement both tiers: **`2` the command line is wrong** (no backend source, an
  unknown flag, a flag missing its value, a malformed `--openapi-header`) and **`1` the
  environment it named is wrong** (missing directory, zero modules, unresolvable spec, missing
  auth key). The distinction is actionable — a supervisor may retry `1` and must never retry
  `2` — and `2` is what argparse, clap and GNU getopt all use, so the bindings agree with the
  tools around them as well as with each other.

  This crate gained `cli::UsageError`, which `main` downcasts to choose the code. `main` now
  returns `ExitCode` rather than `Result`, because Rust's runtime exits `1` for any `Err` and
  would have collapsed the two tiers again.

### Changed

- **`APCoreA2AError` gains a `Config(String)` variant** for misconfiguration detected before
  the server can start (a missing `prefix` in a mixed deployment, an unresolvable spec, a
  document that is not 3.0.x/3.1.x, no usable base URL, an ID collision). Additive;
  `EmptyRegistry` and `Server` are unchanged.

- **`--extensions-dir` is now optional and has no `./extensions` default.** One of it or
  `--from-openapi` is required, and supplying neither is an error naming both. Previously a
  bare `apcore-a2a` served `./extensions`.

### Fixed

- **`--port` was inert.** `cli::run` built its `APCoreA2AConfig` with `..Default::default()`
  and never copied `cli.port`, so the server bound `0.0.0.0:8000` whatever the operator typed,
  with nothing logged. The published `--url` now follows `--port` too (defaulting to
  `http://localhost:<port>`), so the Agent Card cannot advertise a socket the server does not
  bind; an explicit `--url` still wins.

- **The `apcore-a2a.openapi` Config Bus section reached nothing.**
  `build_openapi_backend_from_config` had **no caller anywhere in `src/`** — it was public API
  and a documented first-class surface (feature spec §Configuration, SRS FR-OAS-004 AC 5) that
  no live path read. `cli::run` built its options from the flags alone, so `timeout`,
  `include`, `exclude` and `acknowledge_unapproved_writes` — none of which has a CLI flag in
  any of the three SDKs — were unreachable except by calling the library directly. The CLI now
  merges the section with the flags and routes both the OpenAPI-only and the mixed deployment
  through `build_openapi_backend_from_config`, which also means a relative `spec` resolves
  against `Config::project_root` on the route most deployments use (FR-OAS-004 AC 5).

  The merge is **per key, not per source**: a `--openapi-prefix` alongside a config-declared
  `spec` takes effect, and every config key without a flag survives beside it. Choosing the
  whole source by whoever named `spec` is the shape of the apcore-mcp defect this project
  filed upstream as apcore-mcp-rust#8. The mixed-deployment prefix preflight reads the merged
  value too, so an `apcore-a2a.openapi.prefix` satisfies FR-OAS-006 exactly as the flag does.

- **`--openapi-no-deprecated` could reverse a config setting by being absent.** It was a clap
  `bool`, which collapses "not passed" and "passed false" into the same `false` — so merging
  it would have written `include_deprecated: true` on every run, silently overriding an
  `include_deprecated: false` the operator wrote in the config file. It is now `Option<bool>`
  (`num_args = 0` plus `default_missing_value`, which keeps it a bare flag; `ArgAction::SetTrue`
  does **not** work here, because clap re-applies that action's own `default_value("false")`
  in `Arg::_build` and an absent flag would arrive as `Some(false)`).

- No `--openapi-timeout` flag was added: `timeout` stays Config-Bus-only in all three SDKs.
  What changed is that the configured value now actually reaches the spec fetch.

### The runtime floor (folded in from the unreleased 0.6.1)

The floor also moves to **apcore 0.30.0 / apcore-toolkit 0.11.1**, and the `apcore`
**upper bound is dropped** (`>=0.28, <0.29` -> `>=0.30`). apcore-toolkit 0.11.0 is what
shipped the OpenAPI Scanner this release is built on, so the two are one change.

The bound existed because apcore 0.27 and 0.28 had each shipped an unannounced source
break into downstream `ACLRule` struct literals. apcore 0.29.0 closes that route by making
`ACLRule` `#[non_exhaustive]` — a future field can no longer reach a struct expression at
all — which is exactly what the bound was buying, at the cost of a hand-edit every apcore
minor. `docs/spec/tech-design.md` §13.2 in the spec repo records the reasoning.

Six `ACLRule` struct literals in `tests/integration.rs` accordingly moved to
`ACLRule::new(callers, targets, effect)`, with `approval` and `conditions` assigned
afterwards. **Test-only** — this crate's `src/` constructs no `ACLRule`; it reads an ACL
the host supplies, through `check_access`.

### Upgrade notes

Two apcore 0.29.0 breaks reach a consumer of this crate directly, because both are on
types the consumer holds rather than ones this crate wraps:

- **`ACLRule` is `#[non_exhaustive]`.** Any struct literal building a rule to hand to
  `Executor::set_acl` stops compiling. Migrate to `ACLRule::new` as above.
- **A pattern array with no operands is refused at load.** `callers` / `targets` of `[]`,
  `["$or"]`, `["$not"]` or a multi-operand `["$not", p1, p2]` now raise `ACLRuleError` from
  `ACL::load` / `try_new` / `try_add_rule`, and **panic** from the infallible `ACL::new` and
  `ACL::add_rule`. Such a rule had been contributing nothing to the decision, so under
  `default_effect: "allow"` it permitted the very call it named — and the public Agent Card
  advertised the skill accordingly. See apcore's 0.29.0 changelog for the per-shape
  migration; `["$not", p1, p2]` is the one with no mechanical rewrite.

## [0.6.0] - 2026-09-01

Resolves `aiperceivable/apcore-a2a` issues #2, #3, #4, #5 and `apcore-a2a-rust` #2.
One principle runs through all five: **apcore already draws these distinctions,
and a transport binding's job is to convey them, not to flatten them.**

Suite: 190 tests (was 164).
Runtime floor moves to apcore 0.28.0 / apcore-toolkit 0.10.2 (`apcore = ">=0.28, <0.29"` / `apcore-toolkit = ">=0.10.2"`).

### Changed

- **A governance refusal is reported as itself** (spec srs FR-ERR-003, FR-ERR-009,
  FR-ERR-010, FR-ERR-012). `ACL_DENIED` moves from `-32001 "Task not found"` to
  `-32040 "Access denied"`; `APPROVAL_DENIED` and `APPROVAL_TIMEOUT` leave the
  `-32603` catch-all for `-32041 "Approval denied"` and `-32042 "Approval timed
  out"`. All three now reach `TASK_STATE_REJECTED` instead of
  `TASK_STATE_FAILED`, which matters most on `message/send`, where the response
  is a JSON-RPC `result` and the error code never reaches the caller at all —
  the state and its message are the entire payload.

  The old mapping told an agent a *different* failure had happened, one whose
  correct response was the opposite of the real one: `"Task not found"` sends a
  caller back to re-fetch or re-send the one thing that was fine, and
  `"Internal server error"` is the canonical *retryable* failure — for a call a
  human had explicitly refused. A2A §13.2's MUST NOT forbids revealing *the
  existence of a resource*, not the *class* of failure, so a fixed
  `"Access denied"` naming no caller, target or rule satisfies it while still
  telling an agent to stop.

  `-32001` now means only "unknown task id, or a task owned by another
  principal". `APPROVAL_PENDING` is untouched: still a resumable
  `TASK_STATE_INPUT_REQUIRED` carrying its message verbatim, which is how a
  caller learns the approval id it resumes with.

  **Breaking** for callers that matched `-32001` or the literal `"Task not
  found"` to detect an authorization failure.

- **The public Agent Card shows what an anonymous caller could actually invoke**
  (spec srs FR-AGC-003): every registered skill, minus those the ACL denies to
  the anonymous principal, minus those annotated `requires_approval`. The filter
  resolves one identity, so it runs once at card-build time — never per request
  on the auth-exempt `/.well-known/` route.

- **The extended Agent Card carries what the authenticated caller may invoke**
  (spec srs FR-AGC-004), including `requires_approval` skills, resolved against
  that caller's own identity.

- **`capabilities.extendedAgentCard` is no longer derived from `auth != null`
  alone** (spec srs FR-AGC-002, FR-AGC-006): this binding advertises the
  capability only because it now serves it.

- **Card visibility reads apcore's two governance axes apart** (spec srs
  FR-AGC-003 "The two axes" and criterion 11; FR-AGC-004 criteria 2 and 10).
  apcore 0.28.0 (`PROTOCOL_SPEC` §6.1.6) gave an ACL rule an `approval: required`
  field orthogonal to `effect`, so one check now resolves two independent results
  — may this caller reach this target, and must this call be put to a human — and
  made the legacy boolean `ACL.check` **fail closed** on the second. This binding
  filtered its cards on that boolean. Left alone, a skill the ACL *allows* the
  caller but gates behind a human would have silently vanished from the
  **extended** card too: a refusal the ACL never issued, and the caller left
  unable to learn that a capability it holds exists at all.

  Every card filter now reads `ACL::check_access` and filters on the
  authorization axis alone. The approval axis decides only *which surface*: it
  joins the module's `requires_approval` annotation as the second source the
  public card subtracts, composed by union exactly as apcore §6.9 composes them.
  Since 0.28.0 the annotation describes the *module*, not the call (apcore#110),
  so reading it alone would leave on the public card a skill an anonymous caller
  cannot in fact just call.

  The bug this closes was one line: `handlers::acl_filtered_card` called
  `acl.check(...)`. `ACL::check_access` returns an `AccessDecision`, and the filter
  now reads `decision.is_allowed()` for visibility and `decision.approval_required`
  only to decide which surface — the `hide_approval_gated` argument, `true` for the
  public card and `false` for the extended one. The projection argument is `None`:
  a card is discovery and there is no call site.

  `apcore::acl::ACLRule` also gained a required `approval` field, a source break for
  the struct literals in this crate's integration tests.

- **apcore's `system.*` management namespace never reaches the public Agent Card**
  (spec srs FR-AGC-003 criteria 12 and 13, FR-AGC-004 criterion 11;
  `aiperceivable/apcore-a2a#5`). Removed **unconditionally** — independent of ACL
  state, of the `requires_approval` annotation, and of how `sys_modules` is
  configured. Kept on the extended card, filtered per identity like any other
  skill.

  Every other subtraction the public card makes is governance-shaped, and with no
  ACL configured they all collapse: the ACL predicates are empty and the
  annotation covers only the three `system.control.*` write modules — leaving the
  six read modules, which enumerate the deployment's module inventory, health and
  usage, published to any anonymous caller on the auth-exempt `/.well-known/`
  route. `ACL.discover()` yields nothing for a missing root by design, so "no ACL
  at all" is the default rather than an edge case, and the rule that has to hold
  there cannot be shaped like a governance verdict.

- **Warns when an unprotected control surface is served** (spec srs FR-AGC-007),
  asserted by capturing `tracing` output with a thread-local
  `subscriber::set_default` rather than a global subscriber, so the check cannot
  race the other tests in the integration binary.
  Server construction reads apcore's `Executor::governance_state` and warns when
  `unprotected_control_surface` is true. It never refuses to start and never
  alters a card. Withholding `system.*` from the public card removes the surface
  from *discovery*, not from *dispatch*: apcore's approval gate warns once and
  continues with no `ApprovalHandler`, so the write modules stay callable, and the
  card rule must not be mistaken for a fix to that.

### Added

- **apcore's behavioral annotations reach the wire** (spec srs FR-SKL-004):
  `readonly`, `destructive`, `idempotent` and `requires_approval` are emitted as
  namespaced entries in the standard `tags` field — `apcore:readonly`,
  `apcore:destructive`, `apcore:idempotent`, `apcore:requires-approval` — in
  that fixed order, appended after the module's own tags and de-duplicated
  against them. Only `true` flags are emitted.

  A2A 1.0 `AgentSkill` has no `extensions` and no `metadata` member, so `tags`
  is the only carrier that exists. Without them the card carried enough to
  *construct* a call and not enough to judge whether making it is safe — and
  retry semantics were unusable, since `retryable` is a property of the error
  while whether a retry is safe is a property of the operation.

- **Governance refusal errors on the client**, so a refusal is not reported as
  a transient server failure.

- `A2AClientError::AccessDenied` / `ApprovalDenied` / `ApprovalTimeout`, plus
  `A2AClientError::is_governance_refusal()`.

- **`APCoreA2AConfig::disclose_refusal_reason`** (default `false`, spec srs
  FR-ERR-011): forwards apcore's own sanitized reason for the three governance
  codes instead of the fixed per-class string. The code never changes with the
  flag; only the message does.

- **`GET /agent/authenticatedExtendedCard` and the `GetExtendedAgentCard`
  JSON-RPC method.** This crate advertised `capabilities.extendedAgentCard` and
  routed neither, so a client that read the flag and called the method — which
  A2A §3.2.x entitles it to do — got method-not-found.

### Fixed

- **`sys_modules` registered nothing** (`aiperceivable/apcore-a2a#5`). apcore reads
  `sys_modules.enabled`, a **top-level** config section; the flag was a silent
  no-op in every deployment since it was introduced.

  This binding passed `&Config::default()` to `register_sys_modules`, which reads
  `sys_modules.enabled` and returns an empty context when it is absent — and
  `let _ =` discarded the result, so nothing reported the no-op. The config is now
  built with the key set, the `Result` is inspected, a failure is warned about
  rather than dropped, and the registered ids are logged.

  Fixed together with the namespace rule above, deliberately in that order:
  repairing the config path on its own is precisely what would have opened the
  hole that rule closes.

- **`async_serve` no longer derives its bind address from `config.url`** (#2).
  `APCoreA2AConfig` gains `host` and `port`; `url` is now the Agent Card
  endpoint only, defaulting to `http://{host}:{port}` exactly as the Python and
  TypeScript bindings have always done. The old code string-split `url` on
  `://` and fell back to `0.0.0.0:8000` when the split failed, so a scheme-less
  loopback value like `127.0.0.1:18999` silently published every skill on every
  interface, on a port the operator never chose — with nothing logged. A path, a
  trailing slash or a missing port failed to bind with an address-parse error
  naming neither the URL nor the cause.

  **Breaking**: `APCoreA2AConfig` gains two fields, so struct-literal
  construction must be updated (use `APCoreA2AConfig::default()` with `..`, or
  the builder's new `.host()` / `.port()` / `.bind()` setters).

- Rust now emits the unauthenticated-public-bind warning Python and TypeScript
  already did — the one configuration that most deserves a line in the log
  produced none.

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

- **A JSON-RPC error frame on an SSE stream now raises instead of being yielded
  as an event.** Upstream reports a mid-stream failure as its own frame, tagged
  `event: error` with a JSON-RPC error response in `data:`. Envelope unwrapping
  only looks for `result`, so such a frame fell through and was handed to the
  caller as though it were an event — a caller reading `statusUpdate` saw
  nothing and the failure vanished, while the non-streaming path raised for a
  byte-identical payload. Both paths now share the same error mapping, so a
  `-32001` frame produces `TaskNotFoundError` wherever it arrives. Events
  received before the error frame are still delivered.

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

- **The `tasks/list` method is gone; task listing is `ListTasks` (BREAKING).**
  `tasks/list` was not an A2A method name in any version — 1.0 calls it
  `ListTasks`, and 0.3 had no task-listing method at all, which is why its v0.3
  compatibility layer routes `message/send` / `tasks/get` / `tasks/cancel` but
  not this one. This project invented the name, wrote it into the spec, and
  implemented it only in the Rust server, so the result was broken in both
  directions: the bundled clients' `list_tasks()` returned `-32601` against the
  Python and TypeScript servers, and a third-party 1.0 client's `ListTasks`
  returned `-32601` against the Rust one. Only Rust-server-plus-bundled-client
  worked.

  `A2AClient::list_tasks` now sends `ListTasks` with an `A2A-Version: 1.0`
  header — required, since both upstream SDKs read a request without it as v0.3
  (spec 3.6.2) and reject 1.0 method names with `-32009`. The other methods keep
  their 0.3 spellings, which every SDK still accepts.

  The parameter names were wrong too, which only an end-to-end call could
  surface: `ListTasksRequest` declares `pageSize` / `pageToken` / `contextId` /
  `status` / `historyLength`, and has no `limit` field at all — so even with the
  method name fixed, both SDK-backed servers answered `-32602 Invalid params`.
  The Rust server had never caught it because it ignores list parameters
  entirely. `list_tasks(limit=…)` keeps `limit` as the friendly parameter name
  and sends `pageSize` on the wire.

  **Migration:** callers sending `tasks/list` on the wire must send `ListTasks`
  plus the version header. Users of `A2AClient::list_tasks` need no change. This
  affects Rust deployments only; the method never worked on the other two.

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
