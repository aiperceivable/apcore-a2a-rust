//! OpenAPI backend — serve an OpenAPI 3.0/3.1 document as A2A Skills.
//!
//! Pipeline:
//!
//! ```text
//! load_spec -> OpenAPIScanner::scan -> [repair] -> HTTPProxyRegistryWriter::write -> Registry
//! ```
//!
//! The scanner and the writer both live in apcore-toolkit; this module composes
//! them and adds the two repairs the composition needs, neither of which the
//! toolkit can make on its own:
//!
//! * **FR-OAS-002 module-ID projection.** apcore-toolkit's `derive_module_id`
//!   sanitizes into `[A-Za-z0-9_.-]`; apcore's registry accepts only
//!   `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`. Without the projection the
//!   canonical Swagger Petstore scans cleanly and registers nothing.
//! * **FR-OAS-003 description repair.** An operation carrying neither `summary`
//!   nor `description` yields `""`, and [`AgentCardBuilder`] skips a module
//!   whose description is empty or whitespace-only — so the operation would
//!   vanish from the Agent Card with no diagnostic at all.
//!
//! See `apcore-a2a/docs/features/openapi-backend.md` for the specification and
//! `conformance/fixtures/openapi_backend.json` for the shared contract.
//!
//! [`AgentCardBuilder`]: crate::adapters::AgentCardBuilder

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use apcore::executor::GovernanceState;
use apcore::registry::registry::Registry;
use apcore_toolkit::openapi_scanner::{
    load_spec_with_options, DeriveModuleIdHook, LoadSpecOptions, OpenAPIScanner, ScanOptions,
    TransformModuleHook, TransformOperationHook,
};
use apcore_toolkit::output::http_proxy_writer::HTTPProxyRegistryWriter;
use apcore_toolkit::types::ScannedModule;
use serde_json::Value;

use crate::APCoreA2AError;

/// HTTP methods that change state — the population FR-OAS-005 warns about.
pub const WRITE_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

const URL_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Per-call timeout handed to `HTTPProxyRegistryWriter`, in seconds.
///
/// Deliberately **not** [`OpenAPIBackendOptions::timeout_secs`]: that key is the
/// *spec-fetch* timeout in every SDK (feature spec § Configuration), and a
/// proxy timeout, if one is ever wanted, gets a key of its own. apcore-mcp's
/// Rust binding wires the config value here instead of to the fetch, which
/// makes a documented `timeout: 30.0` configure the opposite thing.
///
/// The value matches apcore-toolkit's own documented writer default
/// (Python's `HTTPProxyRegistryWriter(timeout: float = 60.0)`), which is what
/// the Python binding gets by not passing one; Rust's constructor has no
/// default, so the same number is spelled out here.
const PROXY_TIMEOUT_SECS: f64 = 60.0;

/// Whether one dot-separated segment is a legal apcore module-ID segment.
///
/// apcore's registry enforces `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$` at
/// `Registry::register_module` and again at `Executor::call`. This is that
/// pattern, per segment, hand-rolled so the crate needs no regex for it.
#[must_use]
pub fn is_legal_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Project a toolkit-derived module ID into apcore's registry alphabet.
///
/// Lowercase, then `-` -> `_`. Returns `None` when the result still carries a
/// segment apcore would reject — `/v1/2fa` derives `v1.2fa.get`, and an apcore
/// segment may not begin with a digit, so repairing it would mean *inventing* a
/// character. That is a naming decision belonging to the operator's own
/// `transform_module` hook, not to a silent default: the module is dropped and
/// the caller reports it (FR-OAS-002).
#[must_use]
pub fn project_module_id(module_id: &str) -> Option<String> {
    let candidate = module_id.to_ascii_lowercase().replace('-', "_");
    if candidate.is_empty() {
        return None;
    }
    if candidate.split('.').all(is_legal_segment) {
        Some(candidate)
    } else {
        None
    }
}

/// The first segment of a projected ID that apcore would still reject.
fn offending_segment(module_id: &str) -> String {
    let candidate = module_id.to_ascii_lowercase().replace('-', "_");
    candidate
        .split('.')
        .find(|s| !is_legal_segment(s))
        .unwrap_or(&candidate)
        .to_string()
}

/// Build a `{METHOD} {url_path}` description for an undocumented operation
/// (FR-OAS-003).
///
/// Reads the `http_method` / `url_path` metadata keys `HTTPProxyRegistryWriter`
/// already requires, so the value is factual and stable across scans of the same
/// document. Deliberately terse, so it reads as a placeholder rather than as
/// documentation.
#[must_use]
pub fn synthesize_description(module: &ScannedModule) -> String {
    let method = module
        .metadata
        .get("http_method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    let path = module
        .metadata
        .get("url_path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    match (method.is_empty(), path.is_empty()) {
        (false, false) => format!("{method} {path}"),
        (false, true) => method,
        (true, false) => path,
        (true, true) => {
            if module.module_id.is_empty() {
                "operation".to_string()
            } else {
                module.module_id.clone()
            }
        }
    }
}

/// Resolve the `apcore-a2a.openapi.spec` value (FR-OAS-004).
///
/// `spec` is this namespace's **first path-typed key**, and apcore 0.30.0's
/// protections for path-typed keys do not reach it: `Config::path_typed_keys()`
/// returns a hardcoded set of apcore's own keys and never consults a namespace
/// registered through `Config::register_namespace`, and the PROTOCOL_SPEC
/// §9.2.1 requirement-5 empty-value discard is gated on that same set. So the
/// three rules are this binding's own:
///
/// 1. a value beginning `http://` / `https://` is a URL, used **verbatim** —
///    never path-resolved, never made absolute;
/// 2. a set-but-empty value is discarded with a WARNING and the caller falls
///    through to the next configuration tier. It is never joined to a base,
///    which would silently yield the project root;
/// 3. a relative path resolves against `Config::project_root` — not the process
///    CWD, and not the document's own directory.
///
/// A `project_root` of `None` means "not supplied", not "use the CWD": rule 3
/// is this function's own, so an absent argument falls back to
/// `Config::project_root()` here rather than skipping straight to the process
/// working directory. Both shipped entry points hand it an explicit value, but
/// this is public API — a conformance driver or an embedder calls it directly,
/// and it must not answer differently from its Python and TypeScript siblings,
/// which consult `Config.project_root` from inside the same function. The CWD
/// remains the last resort, for when there is no discoverable config at all.
///
/// Returns `None` when the value was empty and the caller should fall through.
#[must_use]
pub fn resolve_spec_location(spec: &str, project_root: Option<&str>) -> Option<String> {
    if spec.trim().is_empty() {
        tracing::warn!(
            "apcore-a2a.openapi.spec is set but empty; discarding it and falling through to the \
             next configuration tier. An empty value is not a path (mirrors apcore PROTOCOL_SPEC \
             §9.2.1 requirement 5, whose fixed key set does not reach this namespace)."
        );
        return None;
    }
    if URL_SCHEMES.iter().any(|s| spec.starts_with(s)) {
        return Some(spec.to_string());
    }
    let path = Path::new(spec);
    if path.is_absolute() {
        return Some(spec.to_string());
    }
    let base: PathBuf = project_root
        .map(PathBuf::from)
        // Lazy, so a URL or an absolute path never pays for a config discovery,
        // and so the CWD is reached only when there is no config to read.
        .or_else(|| config_project_root().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    Some(normalize(&base.join(path)))
}

/// Lexically normalize `.` / `..` without touching the filesystem, so the result
/// is comparable across the three SDKs for a path that need not exist.
fn normalize(path: &Path) -> String {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    let mut buf = PathBuf::new();
    for part in out {
        buf.push(part);
    }
    buf.to_string_lossy().into_owned()
}

/// `Config::project_root()` for the discovered configuration, as a string.
///
/// The Config-Bus route resolves this itself rather than leaving
/// [`OpenAPIBackendOptions::project_root`] unset: that route is the one most
/// deployments use, and leaving it unset is exactly what makes apcore-mcp's
/// TypeScript and Rust config routes resolve a relative `spec` against the CWD,
/// contradicting FR-OAS-004 rule 3.
fn config_project_root() -> Option<String> {
    match apcore::config::Config::discover() {
        Ok(config) => Some(config.project_root().to_string_lossy().into_owned()),
        Err(error) => {
            tracing::debug!(%error, "could not read Config::project_root; falling back to CWD");
            None
        }
    }
}

/// Options for [`openapi_backend`] and [`openapi_backend_from_spec`].
///
/// [`OpenAPIBackendOptions::new`] and `Default::default()` are the same value,
/// and `..Default::default()` is safe as the base of a struct-update
/// expression. That is deliberate rather than incidental: two of the documented
/// defaults are **not** the corresponding zero value (`include_deprecated` is
/// `true`, `timeout_secs` is `30.0`), so a derived `Default` would hand the
/// idiomatic `..Default::default()` spelling a **zero-second spec-fetch
/// timeout** and silently drop every `deprecated: true` operation. Python and
/// TypeScript put these defaults in the function signature and structurally
/// cannot disagree with themselves; this impl is how Rust matches them.
pub struct OpenAPIBackendOptions {
    /// Where proxied requests go. Defaults to the document's `servers[0].url`.
    pub base_url: Option<String>,
    /// Prepended to every derived module ID. Required in a mixed deployment.
    pub prefix: Option<String>,
    /// Scanner include filter.
    pub include: Option<String>,
    /// Scanner exclude filter.
    pub exclude: Option<String>,
    /// When `false`, `deprecated: true` operations are skipped. Default `true`.
    pub include_deprecated: bool,
    /// Headers sent with the **spec fetch** only, never with a proxied call: a
    /// document is often public while the API behind it is not, and reusing a
    /// spec-read key on every skill invocation would be a privilege escalation
    /// the operator never wrote down. They are secrets — never logged, never
    /// echoed into the Agent Card, never named in a fetch-failure message.
    pub headers: Option<HashMap<String, String>>,
    /// **Spec-fetch** timeout in seconds. Default `30.0`. Not the per-call proxy
    /// timeout — see [`PROXY_TIMEOUT_SECS`].
    pub timeout_secs: f64,
    /// Per-request auth headers for proxied calls, invoked once per request.
    /// The correct home for a rotating token, and the reason there is no CLI
    /// flag for upstream credentials.
    pub auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    /// True when another backend source is configured, which makes `prefix`
    /// mandatory (FR-OAS-006).
    pub has_other_backend_source: bool,
    /// `Config::project_root` (apcore 0.30.0). A relative `spec` resolves here.
    pub project_root: Option<String>,
    /// Records an explicit operator decision to advertise write-method
    /// operations with no approval path on the public card. The only
    /// configuration flag that suppresses the FR-OAS-005 warning.
    pub acknowledge_unapproved_writes: bool,
    /// `Executor::governance_state()`, when the caller happens to hold an
    /// executor already. Only *escalates* the FR-OAS-005 warning — when
    /// `builtin_approval_gate_wired` is `false` the approval gate is not in the
    /// running pipeline at all — and never suppresses it.
    pub governance_state: Option<GovernanceState>,
    /// The caller's own per-operation hook. Runs **first**, before the
    /// description repair and the ID projection, so the two invariants those
    /// repairs hold ("every registered ID is apcore-legal", "every registered
    /// module has a non-empty description") hold unconditionally whatever this
    /// returns.
    ///
    /// Spelled with apcore-toolkit's own `TransformModuleHook` alias, so a hook
    /// written against the scanner drops in here unchanged.
    pub transform_module: Option<TransformModuleHook>,
    /// The scanner's `transform_operation` hook, forwarded unchanged.
    pub transform_operation: Option<TransformOperationHook>,
    /// The scanner's `derive_module_id` hook, forwarded unchanged.
    pub derive_module_id: Option<DeriveModuleIdHook>,
}

impl Default for OpenAPIBackendOptions {
    /// Every field at its **documented** default — not at its zero value.
    ///
    /// Written out by hand rather than derived. `#[derive(Default)]` produces
    /// `include_deprecated: false` and `timeout_secs: 0.0`, and neither is a
    /// value any operator asked for: a zero spec-fetch timeout fails every
    /// fetch, and the derive contradicts the two keys the feature spec's
    /// Configuration table documents as `true` / `30.0`. A doc comment saying
    /// "use `new()`" cannot enforce that, because `..Default::default()` is the
    /// idiomatic spelling of a struct update and reaches the derive silently.
    fn default() -> Self {
        Self {
            base_url: None,
            prefix: None,
            include: None,
            exclude: None,
            include_deprecated: true,
            headers: None,
            timeout_secs: 30.0,
            auth_header_factory: None,
            has_other_backend_source: false,
            project_root: None,
            acknowledge_unapproved_writes: false,
            governance_state: None,
            transform_module: None,
            transform_operation: None,
            derive_module_id: None,
        }
    }
}

impl OpenAPIBackendOptions {
    /// Options with every field at its documented default.
    ///
    /// Identical to [`OpenAPIBackendOptions::default`]; kept as the named
    /// constructor the other two SDKs' call sites read like.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for OpenAPIBackendOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `headers` is deliberately rendered as a count, never as content: the
        // spec-fetch headers are credentials and must not reach a log line
        // through a stray `{:?}`.
        f.debug_struct("OpenAPIBackendOptions")
            .field("base_url", &self.base_url)
            .field("prefix", &self.prefix)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("include_deprecated", &self.include_deprecated)
            .field("headers", &self.headers.as_ref().map(HashMap::len))
            .field("timeout_secs", &self.timeout_secs)
            .field(
                "auth_header_factory",
                &self.auth_header_factory.as_ref().map(|_| "<factory>"),
            )
            .field("has_other_backend_source", &self.has_other_backend_source)
            .field("project_root", &self.project_root)
            .field(
                "acknowledge_unapproved_writes",
                &self.acknowledge_unapproved_writes,
            )
            .field("governance_state", &self.governance_state)
            .finish_non_exhaustive()
    }
}

/// Build a [`Registry`] from a spec **location** — a URL or a filesystem path —
/// resolving and fetching it first.
///
/// This is the async wrapper the other two SDKs fold into their single
/// polymorphic `spec` parameter: apcore-toolkit-rust's `load_spec` is `async`,
/// so the fetch cannot hide inside [`openapi_backend`]'s document argument.
///
/// # Errors
///
/// Returns [`APCoreA2AError::Config`] when `spec` resolves to nothing (a
/// set-but-empty value with no fallback tier), when the document cannot be
/// fetched or parsed, and for every error [`openapi_backend`] itself raises.
pub async fn openapi_backend_from_spec(
    spec: &str,
    registry: Arc<Registry>,
    options: OpenAPIBackendOptions,
) -> Result<Arc<Registry>, APCoreA2AError> {
    // Checked before the fetch as well as inside `openapi_backend`, so a mixed
    // deployment missing its prefix fails without a network round trip
    // (FR-OAS-006: "checked FIRST, before any fetch or scan").
    require_prefix(&options)?;

    // An unset `project_root` falls back to `Config::project_root()` rather than
    // straight to the CWD, so the programmatic and CLI routes resolve a relative
    // spec against the same base the Config-Bus route does (FR-OAS-004 rule 3).
    // That fallback lives in `resolve_spec_location` itself — the function is
    // public, so a driver calling it directly must get the same answer this
    // entry point does; a copy of the fallback here is how the two drift apart.
    let resolved =
        resolve_spec_location(spec, options.project_root.as_deref()).ok_or_else(|| {
            APCoreA2AError::Config(
                "apcore-a2a.openapi.spec is required and resolved to nothing.".to_string(),
            )
        })?;

    let load_options = LoadSpecOptions {
        // Threaded through rather than dropped: apcore-mcp's Rust binding has no
        // field for these at all, which makes its `--openapi-header` flag a
        // silent no-op.
        headers: options.headers.clone(),
        auth_header_factory: None,
        // The SPEC-FETCH timeout. See `PROXY_TIMEOUT_SECS`.
        timeout_secs: options.timeout_secs,
    };

    // `load_spec_with_options` handles both branches (URL vs local path) and
    // both JSON and YAML parsing. The error names the resolved location and
    // never the headers, which are credentials.
    let document = load_spec_with_options(&resolved, &load_options)
        .await
        .map_err(|e| {
            APCoreA2AError::Config(format!(
                "apcore-a2a.openapi: failed to load spec '{resolved}': {e}"
            ))
        })?;

    openapi_backend(&document, registry, options).await
}

/// Build a [`Registry`] from an already-parsed OpenAPI 3.0/3.1 document.
///
/// See `Contract: openapi_backend` in
/// `apcore-a2a/docs/features/openapi-backend.md`. Returns the same `registry`
/// it was handed, populated. Never partially populated: the collision preflight
/// and every validation run before the first write.
///
/// # Errors
///
/// Returns [`APCoreA2AError::Config`] when `prefix` is required and absent, when
/// the document is not OpenAPI 3.0.x/3.1.x, when no base URL can be determined,
/// or when a derived module ID collides with one already in `registry`.
pub async fn openapi_backend(
    document: &Value,
    registry: Arc<Registry>,
    options: OpenAPIBackendOptions,
) -> Result<Arc<Registry>, APCoreA2AError> {
    require_prefix(&options)?;

    // Collected through an `Arc<Mutex<_>>` because `TransformModuleHook` is
    // `Send + Sync`: the hook cannot borrow a local `Vec`, and a hook returning
    // `None` drops the module SILENTLY, so reporting is this function's job and
    // cannot be delegated to the scanner.
    let dropped: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let synthesized: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let dropped_hook = Arc::clone(&dropped);
    let synthesized_hook = Arc::clone(&synthesized);
    let caller_hook = options.transform_module;

    let mut scan_options = ScanOptions::new();
    scan_options.include = options.include;
    scan_options.exclude = options.exclude;
    scan_options.base_path_prefix = options.prefix;
    scan_options.include_deprecated = options.include_deprecated;
    scan_options.transform_operation = options.transform_operation;
    scan_options.derive_module_id = options.derive_module_id;
    scan_options.transform_module = Some(Box::new(move |module: ScannedModule| {
        // 1. The caller's own hook FIRST, so the invariants below hold
        //    unconditionally, whatever it returns.
        let mut module = match &caller_hook {
            Some(hook) => hook(module)?,
            None => module,
        };

        // 2. FR-OAS-003: repair the description before the module can reach a
        //    card filter that would silently drop it.
        let was_synthesized = module.description.trim().is_empty();
        if was_synthesized {
            module.description = synthesize_description(&module);
        }

        // 3. FR-OAS-002 LAST, so "every registered module ID is apcore-legal"
        //    holds unconditionally. It still runs BEFORE the scanner's own
        //    `deduplicate_ids` — which happens after this callback — because
        //    lowercasing can CREATE a collision the document did not have:
        //    `listPets` and `listpets` are two operations to OpenAPI and one
        //    module ID to apcore.
        let Some(projected) = project_module_id(&module.module_id) else {
            if let Ok(mut guard) = dropped_hook.lock() {
                guard.push((
                    module.module_id.clone(),
                    offending_segment(&module.module_id),
                ));
            }
            return None;
        };
        module.module_id = projected;

        // Report the POST-projection id: it is the one that reaches the Agent
        // Card, and a diagnostic naming the pre-projection id sends the operator
        // looking for a skill that does not exist.
        if was_synthesized {
            if let Ok(mut guard) = synthesized_hook.lock() {
                guard.push(module.module_id.clone());
            }
        }
        Some(module)
    }));

    let modules = OpenAPIScanner::new()
        .scan(document, &scan_options)
        .await
        .map_err(|e| APCoreA2AError::Config(format!("apcore-a2a.openapi: {e}")))?;

    // --- Diagnostics, in the order the feature spec's "Emits" list states -----
    let dropped = take(&dropped);
    for (derived_id, segment) in &dropped {
        tracing::warn!(
            "apcore-a2a: skipping OpenAPI operation '{derived_id}' — the derived module ID has a \
             segment ('{segment}') apcore's registry cannot accept (it must match \
             ^[a-z][a-z0-9_]*$), and it cannot be repaired without inventing an ID. Supply a \
             derive_module_id or transform_module hook to name this operation yourself."
        );
    }
    for module in &modules {
        for warning in &module.warnings {
            tracing::warn!("apcore-a2a: {}: {warning}", module.module_id);
        }
    }
    if modules.is_empty() {
        tracing::warn!(
            "apcore-a2a: the OpenAPI document produced no registrable modules; the Agent Card will \
             have no skills."
        );
    }
    let mut synthesized = take(&synthesized);
    if !synthesized.is_empty() {
        synthesized.sort();
        tracing::info!(
            "apcore-a2a: {} of {} scanned operations had no summary or description; a \
             \"{{METHOD}} {{path}}\" description was synthesized so they appear on the Agent Card. \
             Affected: {}",
            synthesized.len(),
            modules.len() + dropped.len(),
            synthesized.join(", ")
        );
    }

    // --- Collision preflight, over the FULL set, before the first write ------
    // Toolkit writers report per-module `WriteResult`s and never abort, so
    // without this a duplicate arrives as a failed WriteResult, gets logged and
    // skipped, and leaves a partial registry: a skill the document advertises,
    // absent from the card, with one log line as the only notice.
    let existing = registry.list(None, None, Some(&["public", "hidden"]));
    let mut collisions: Vec<String> = modules
        .iter()
        .map(|m| m.module_id.clone())
        .filter(|id| existing.contains(id))
        .collect();
    collisions.sort_unstable();
    collisions.dedup();
    if !collisions.is_empty() {
        // EVERY colliding ID, sorted and deduplicated: reporting only the first
        // forces one restart per collision.
        return Err(APCoreA2AError::Config(format!(
            "OpenAPI-derived module IDs collide with modules already in the registry: {}. Nothing \
             was registered. Set apcore-a2a.openapi.prefix to namespace them.",
            collisions.join(", ")
        )));
    }

    // --- Base URL -----------------------------------------------------------
    let base_url = options
        .base_url
        .or_else(|| document_server_url(document))
        .ok_or_else(|| {
            APCoreA2AError::Config(
                // "has no", not "declares no": Python and TypeScript both say
                // "has no", and a diagnostic an operator greps for across three
                // SDKs must be one string. The rest of this message — the
                // unknown-host consequence and the config key — is the canonical
                // wording the other two adopted.
                "No base_url: the document has no usable absolute servers[0].url, so every \
                 proxied call would resolve against an unknown host. Set \
                 apcore-a2a.openapi.base_url."
                    .to_string(),
            )
        })?;

    // --- Write --------------------------------------------------------------
    let writer = HTTPProxyRegistryWriter::new(
        base_url,
        options.auth_header_factory,
        // NOT `options.timeout_secs` — that is the spec-fetch timeout.
        PROXY_TIMEOUT_SECS,
    )
    .map_err(|e| APCoreA2AError::Config(format!("apcore-a2a.openapi: {e}")))?;

    for result in writer.write(&modules, &registry) {
        if let Some(error) = result.verification_error {
            tracing::error!(
                "apcore-a2a: {} failed to register as an HTTP proxy: {error}",
                result.module_id
            );
        }
    }

    warn_unapproved_writes(
        &modules,
        options.acknowledge_unapproved_writes,
        options.governance_state.as_ref(),
    );
    Ok(registry)
}

/// Drain an `Arc<Mutex<Vec<_>>>` collected by the scan hook.
fn take<T>(cell: &Arc<Mutex<Vec<T>>>) -> Vec<T> {
    cell.lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// FR-OAS-006, checked before any fetch or scan.
fn require_prefix(options: &OpenAPIBackendOptions) -> Result<(), APCoreA2AError> {
    if options.has_other_backend_source && options.prefix.as_deref().unwrap_or("").is_empty() {
        return Err(APCoreA2AError::Config(
            "apcore-a2a.openapi.prefix is required when another backend source is also \
             configured: the scanner deduplicates IDs within one scan only and knows nothing \
             about modules already in the registry, so a derived module ID can collide with a \
             project module ID. Set --openapi-prefix / apcore-a2a.openapi.prefix."
                .to_string(),
        ));
    }
    Ok(())
}

/// `servers[0].url`, when it is a usable absolute URL.
fn document_server_url(document: &Value) -> Option<String> {
    document
        .get("servers")?
        .as_array()?
        .first()?
        .get("url")?
        .as_str()
        .filter(|u| URL_SCHEMES.iter().any(|s| u.starts_with(s)))
        .map(String::from)
}

/// FR-OAS-005 — the unapproved-write warning.
///
/// Reports the **absence of a gate, never the presence of protection**, which is
/// the rule apcore states on `GovernanceState::unprotected_control_surface`: a
/// wired ACL that permits every call still yields `false`. So an attached ACL
/// never suppresses this warning.
///
/// Exactly **two** tiers, and the wording of each is deliberate:
///
/// * escalated when `builtin_approval_gate_wired` is `false` — under the
///   `internal`, `testing` and `minimal` strategies the approval gate is not in
///   the pipeline at all, so neither a module annotation nor an ACL rule's
///   `approval: required` would fire;
/// * otherwise the base wording, which speaks only to the **module-level**
///   declaration. There is deliberately no "an ACL approval path exists" tier:
///   when this runs, the `Executor` and its ACL do not exist yet — the backend
///   builds the `Registry` the `Executor` is later constructed *from* — so
///   "does some ACL rule carry `approval: required`" is genuinely unknowable
///   here rather than merely unimplemented. That question belongs to the
///   serve-time FR-AGC-007 warning, which runs where the executor does exist.
///   (apcore's `GovernanceState` carries no `approval_rule_present` field
///   either, so a port that reads one gets a branch that can never be taken.)
///
/// The message names the **public** Agent Card because that is the exposure that
/// distinguishes this binding from apcore-mcp's tool list and apcore-cli's local
/// command surface: `/.well-known/agent-card.json` is auth-exempt by design and
/// A2A clients are built to crawl it.
fn warn_unapproved_writes(
    modules: &[ScannedModule],
    acknowledge: bool,
    governance_state: Option<&GovernanceState>,
) {
    if acknowledge {
        return;
    }

    let unapproved: Vec<&ScannedModule> = modules
        .iter()
        .filter(|m| {
            let method = m
                .metadata
                .get("http_method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            WRITE_METHODS.contains(&method.as_str())
                && !m.annotations.as_ref().is_some_and(|a| a.requires_approval)
        })
        .collect();
    if unapproved.is_empty() {
        return;
    }

    let mut methods: Vec<String> = unapproved
        .iter()
        .map(|m| {
            m.metadata
                .get("http_method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase()
        })
        .collect();
    methods.sort();
    methods.dedup();

    let mut ids: Vec<&str> = unapproved.iter().map(|m| m.module_id.as_str()).collect();
    ids.sort_unstable();
    ids.truncate(10);

    let gate_unwired = governance_state.is_some_and(|g| !g.builtin_approval_gate_wired);
    let lead = if gate_unwired {
        "the approval gate is NOT in the execution pipeline under the active strategy, so neither \
         a module annotation nor an ACL rule's `approval: required` would fire for"
    } else {
        "no module-level approval requirement is declared for"
    };

    tracing::warn!(
        "apcore-a2a: {lead} {} scanned write operation(s) ({}). They will be advertised on the \
         PUBLIC Agent Card at /.well-known/agent-card.json, which is served without \
         authentication. Configure an ACL rule carrying `approval: required`, set \
         requires_approval via a transform_module hook, or set \
         apcore-a2a.openapi.acknowledge_unapproved_writes: true to record this as intended. \
         Affected: {}",
        unapproved.len(),
        methods.join("/"),
        ids.join(", ")
    );
}

/// Translate an `apcore-a2a.openapi` Config Bus mapping into a spec value and
/// [`OpenAPIBackendOptions`].
///
/// Split out from [`build_openapi_backend_from_config`] so the `project_root`
/// wiring is unit-testable without a network fetch: leaving it unset is the
/// defect that makes apcore-mcp's TypeScript and Rust config routes resolve a
/// relative `spec` against the CWD.
///
/// `Ok(None)` means **no OpenAPI is configured**, which is an ordinary outcome
/// and not an error — `openapi: null` is the registered namespace's own
/// default, so the absent case arrives here on every deployment that never set
/// the section. Python returns `None` and TypeScript `null` for it. A
/// wrong-*shaped* value (a string, a number, an array) stays an error naming the
/// shape, because that one is a typo the operator has to see.
fn options_from_config(
    openapi_config: &Value,
    has_other_backend_source: bool,
    governance_state: Option<GovernanceState>,
) -> Result<Option<(Value, OpenAPIBackendOptions)>, APCoreA2AError> {
    if openapi_config.is_null() {
        return Ok(None);
    }
    let obj = openapi_config.as_object().ok_or_else(|| {
        APCoreA2AError::Config(format!(
            "apcore-a2a.openapi must be a mapping, got {}",
            value_type_name(openapi_config)
        ))
    })?;
    let spec = obj
        .get("spec")
        .filter(|v| !v.is_null())
        .filter(|v| !matches!(v, Value::String(s) if s.trim().is_empty()))
        .ok_or_else(|| {
            APCoreA2AError::Config(
                "apcore-a2a.openapi.spec is required when apcore-a2a.openapi is configured."
                    .to_string(),
            )
        })?;

    let headers = obj.get("headers").and_then(Value::as_object).map(|map| {
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<HashMap<String, String>>()
    });

    let options = OpenAPIBackendOptions {
        base_url: obj
            .get("base_url")
            .and_then(Value::as_str)
            .map(String::from),
        prefix: obj.get("prefix").and_then(Value::as_str).map(String::from),
        include: obj.get("include").and_then(Value::as_str).map(String::from),
        exclude: obj.get("exclude").and_then(Value::as_str).map(String::from),
        include_deprecated: obj
            .get("include_deprecated")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        headers,
        timeout_secs: obj.get("timeout").and_then(Value::as_f64).unwrap_or(30.0),
        has_other_backend_source,
        // Resolved HERE rather than left to the default (FR-OAS-004 rule 3).
        project_root: config_project_root(),
        acknowledge_unapproved_writes: obj
            .get("acknowledge_unapproved_writes")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Forwarded, never defaulted. It is the ONLY input that can reach
        // FR-OAS-005's escalated tier (`builtin_approval_gate_wired == false`,
        // i.e. the approval gate is not in the pipeline at all under the
        // `internal` / `testing` / `minimal` strategies), so dropping it here
        // makes that tier unreachable on the route most deployments use, while
        // the base warning still fires and reads as if the gate were present.
        governance_state,
        // `auth_header_factory` is deliberately not read from the Config Bus: it
        // is a closure, and a value sourced from YAML/JSON/env can never carry
        // one. The same holds for the three scanner hooks.
        ..OpenAPIBackendOptions::new()
    };
    Ok(Some((spec.clone(), options)))
}

/// Build the backend from an `apcore-a2a.openapi` Config Bus section.
///
/// Returns `Ok(None)` when the section is absent — `openapi_config` is
/// `Value::Null`, which is the registered namespace's own default — so a caller
/// can treat "no OpenAPI configured" as an ordinary outcome instead of an error
/// it has to pattern-match out of a message. Python returns `None` here and
/// TypeScript `null`; before this the Rust route had no way to say it, and an
/// explicit `openapi: null` surfaced as "apcore-a2a.openapi must be a mapping,
/// got null".
///
/// `governance_state` is forwarded to FR-OAS-005's warning, where it can only
/// *escalate*: `builtin_approval_gate_wired == false` means the approval gate is
/// not in the running pipeline, so neither a module annotation nor an ACL rule's
/// `approval: required` would fire. It never suppresses the warning. Pass `None`
/// when no `Executor` exists yet — which is the usual case on this route, since
/// the registry this builds is what the executor is later constructed from.
///
/// # Errors
///
/// Returns [`APCoreA2AError::Config`] when the section is present but not a
/// mapping, when it carries no usable `spec`, and for every error
/// [`openapi_backend_from_spec`] / [`openapi_backend`] can raise.
pub async fn build_openapi_backend_from_config(
    openapi_config: &Value,
    registry: Arc<Registry>,
    has_other_backend_source: bool,
    governance_state: Option<GovernanceState>,
) -> Result<Option<Arc<Registry>>, APCoreA2AError> {
    let Some((spec, options)) =
        options_from_config(openapi_config, has_other_backend_source, governance_state)?
    else {
        return Ok(None);
    };
    match spec {
        Value::String(s) => openapi_backend_from_spec(&s, registry, options).await,
        document => openapi_backend(&document, registry, options).await,
    }
    .map(Some)
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_camel_case_and_hyphens() {
        assert_eq!(project_module_id("listPets").as_deref(), Some("listpets"));
        assert_eq!(
            project_module_id("pet-store.items.get").as_deref(),
            Some("pet_store.items.get")
        );
        assert_eq!(
            project_module_id("already.legal").as_deref(),
            Some("already.legal")
        );
        assert_eq!(
            project_module_id("Users.UserId.Get").as_deref(),
            Some("users.userid.get")
        );
    }

    #[test]
    fn refuses_a_segment_that_cannot_be_repaired() {
        // An apcore segment may not begin with a digit, and prefixing one would
        // be inventing an ID rather than projecting the derived one.
        assert_eq!(project_module_id("v1.2fa.get"), None);
        assert_eq!(project_module_id("9lives"), None);
        assert_eq!(project_module_id(""), None);
        assert_eq!(offending_segment("v1.2fa.get"), "2fa");
    }

    #[test]
    fn spec_location_rules() {
        // 1. a URL verbatim, never joined to the project root.
        assert_eq!(
            resolve_spec_location("https://api.example.com/openapi.json", Some("/srv/project"))
                .as_deref(),
            Some("https://api.example.com/openapi.json")
        );
        // 2. a set-but-empty value discarded, so the caller falls through.
        assert_eq!(resolve_spec_location("", Some("/srv/project")), None);
        assert_eq!(resolve_spec_location("   ", Some("/srv/project")), None);
        // 3. a relative path against the project root, an absolute one untouched.
        assert_eq!(
            resolve_spec_location("./openapi.json", Some("/srv/project")).as_deref(),
            Some("/srv/project/openapi.json")
        );
        assert_eq!(
            resolve_spec_location("/etc/apcore/openapi.json", Some("/srv/project")).as_deref(),
            Some("/etc/apcore/openapi.json")
        );
    }

    #[test]
    fn synthesizes_method_and_path() {
        let mut module = ScannedModule::new(
            "deletepet".to_string(),
            String::new(),
            json!({}),
            json!({}),
            vec![],
            "DELETE /pets/{petId}".to_string(),
        );
        module
            .metadata
            .insert("http_method".to_string(), json!("DELETE"));
        module
            .metadata
            .insert("url_path".to_string(), json!("/pets/{petId}"));
        assert_eq!(synthesize_description(&module), "DELETE /pets/{petId}");

        // No metadata at all: fall back to something nameable rather than "".
        let bare = ScannedModule::new(
            "op".to_string(),
            String::new(),
            json!({}),
            json!({}),
            vec![],
            String::new(),
        );
        assert_eq!(synthesize_description(&bare), "op");
    }

    #[test]
    fn config_route_resolves_the_project_root() {
        // The apcore-mcp defect this pins: its `build_openapi_backend_from_config`
        // sets `project_root: None`, so a relative `spec` silently resolves
        // against the CWD on the one route most deployments use.
        let (spec, options) = options_from_config(
            &json!({ "spec": "./openapi.json", "timeout": 5.0 }),
            false,
            None,
        )
        .expect("options")
        .expect("a spec was named");
        assert_eq!(spec, json!("./openapi.json"));
        assert!(
            options.project_root.is_some(),
            "the config route must resolve Config::project_root itself"
        );
        assert_eq!(options.timeout_secs, 5.0);
        assert!(options.include_deprecated);
    }

    #[test]
    fn config_route_threads_headers_through() {
        // apcore-mcp's Rust options struct has no `headers` field at all, which
        // makes its `--openapi-header` flag a silent no-op.
        let (_spec, options) = options_from_config(
            &json!({ "spec": "https://api.example.com/o.json", "headers": { "X-Api-Key": "k" } }),
            false,
            None,
        )
        .expect("options")
        .expect("a spec was named");
        assert_eq!(
            options.headers.as_ref().and_then(|h| h.get("X-Api-Key")),
            Some(&"k".to_string())
        );
        // ... and never renders them: spec-fetch headers are secrets and must
        // not reach a log line through a stray `{:?}`.
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("X-Api-Key"), "{rendered}");
        assert!(!rendered.contains("\"k\""), "{rendered}");
    }

    #[test]
    fn config_route_rejects_a_missing_or_empty_spec() {
        assert!(options_from_config(&json!({}), false, None).is_err());
        assert!(options_from_config(&json!({ "spec": "" }), false, None).is_err());
        assert!(options_from_config(&json!({ "spec": null }), false, None).is_err());
        assert!(options_from_config(&json!("not-a-mapping"), false, None).is_err());
    }

    #[test]
    fn an_absent_openapi_section_is_an_ordinary_outcome_not_an_error() {
        // `openapi: null` IS the registered namespace default, so a project that
        // simply has not configured an OpenAPI backend hits this on every start.
        // Returning `Err` made "not configured" indistinguishable from
        // "misconfigured", and the old return type could not express the
        // difference at all — Python returns `None` and TypeScript `null` here.
        assert!(
            matches!(options_from_config(&json!(null), false, None), Ok(None)),
            "an explicit null section must be an absence, not an error"
        );

        // A genuinely wrong shape stays an error that names the shape. That is a
        // different question from "is one configured", and conflating the two is
        // what sent the operator looking for a missing flag.
        let wrong = options_from_config(&json!("./spec.json"), false, None);
        assert!(
            wrong.is_err(),
            "a wrong-shaped section must still be an error"
        );
        assert!(
            format!("{}", wrong.unwrap_err()).contains("must be a mapping"),
            "and must still name the shape"
        );
    }

    #[test]
    fn default_matches_new_rather_than_the_derive() {
        // `#[derive(Default)]` gave `include_deprecated: false` and
        // `timeout_secs: 0.0`, and `..Default::default()` is the idiomatic
        // struct-update spelling — so the derive silently produced a ZERO-second
        // spec-fetch timeout for anyone who reached for it. Python and TypeScript
        // put their defaults in the signature and structurally cannot have this.
        let derived = OpenAPIBackendOptions::default();
        let constructed = OpenAPIBackendOptions::new();
        assert_eq!(derived.include_deprecated, constructed.include_deprecated);
        assert_eq!(derived.timeout_secs, constructed.timeout_secs);
        assert!(derived.include_deprecated, "the spec default is true");
        assert_eq!(
            derived.timeout_secs, 30.0,
            "the spec default is 30.0 seconds"
        );
    }

    #[test]
    fn mixed_deployment_requires_a_prefix_before_any_fetch() {
        let mut options = OpenAPIBackendOptions::new();
        options.has_other_backend_source = true;
        let err = require_prefix(&options).expect_err("prefix required");
        assert!(err.to_string().contains("prefix"), "{err}");

        options.prefix = Some("petstore".to_string());
        assert!(require_prefix(&options).is_ok());
    }
}
