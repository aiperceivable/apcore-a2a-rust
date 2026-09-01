//! APCoreA2A — central orchestrator and builder.

use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

use apcore::config::Config;
use apcore::executor::Executor;
use apcore::registry::registry::Registry;
use axum::Router;

use crate::adapters::agent_card::AgentCard;
use crate::adapters::parts::PartConverter;
use crate::adapters::schema::SchemaConverter;
use crate::auth::protocol::Authenticator;
use crate::server::executor::ApCoreAgentExecutor;
use crate::server::factory::{A2AServerFactory, CreateOptions};

#[derive(Debug, Error)]
pub enum APCoreA2AError {
    #[error("no modules found in registry")]
    EmptyRegistry,
    #[error("server error: {0}")]
    Server(String),
}

pub enum BackendSource {
    ExtensionsDir(PathBuf),
    Registry(Arc<Registry>),
    Executor(Arc<Executor>),
}

impl From<&str> for BackendSource {
    fn from(s: &str) -> Self {
        BackendSource::ExtensionsDir(PathBuf::from(s))
    }
}

impl From<PathBuf> for BackendSource {
    fn from(p: PathBuf) -> Self {
        BackendSource::ExtensionsDir(p)
    }
}

#[derive(Debug, Clone)]
pub struct APCoreA2AConfig {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Bind host. Separate from [`url`](Self::url), which is what the Agent Card
    /// publishes: those differ whenever a proxy, a container port mapping or a
    /// public hostname is involved, which is the normal case in any real
    /// deployment. Matches `host` in the Python and TypeScript bindings.
    pub host: String,
    /// Bind port. Matches `port` in the Python and TypeScript bindings.
    pub port: u16,
    /// Public endpoint published in the Agent Card's `supportedInterfaces[].url`.
    /// **Not** the socket to bind — see [`host`](Self::host) / [`port`](Self::port).
    /// Empty means "derive from host and port", as Python's
    /// `url: str | None = None` documents.
    pub url: String,
    pub execution_timeout: u64,
    pub explorer: bool,
    pub metrics: bool,
    /// Register apcore `sys.*` modules (health/usage/manifest). Off by default.
    pub sys_modules: bool,
    /// Allowed CORS origins. Empty = no CORS layer.
    pub cors_origins: Vec<String>,
    /// Forward apcore's own reason for a governance refusal instead of the fixed
    /// per-class string (srs FR-ERR-011). Off by default.
    ///
    /// The *class* of refusal is conveyed either way — `Access denied`,
    /// `Approval denied`, `Approval timed out`, each with its own JSON-RPC code.
    /// This flag decides only whether the *detail* travels with it. A server
    /// whose callers are its own agents wants it on: that is what the apcore MCP
    /// binding reports today, so an operator comparing the two transports
    /// otherwise sees the reason on one and not the other.
    pub disclose_refusal_reason: bool,
}

impl Default for APCoreA2AConfig {
    fn default() -> Self {
        Self {
            name: "apcore-a2a".to_string(),
            description: "apcore A2A agent".to_string(),
            version: crate::VERSION.to_string(),
            host: "0.0.0.0".to_string(),
            port: 8000,
            url: "http://localhost:8000".to_string(),
            execution_timeout: 300,
            explorer: false,
            metrics: false,
            sys_modules: false,
            cors_origins: vec![],
            disclose_refusal_reason: false,
        }
    }
}

pub struct APCoreA2A {
    config: APCoreA2AConfig,
}

pub struct APCoreA2ABuilder {
    config: APCoreA2AConfig,
}

impl APCoreA2ABuilder {
    pub fn new() -> Self {
        Self {
            config: APCoreA2AConfig::default(),
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.config.description = desc.into();
        self
    }

    /// The endpoint published in the Agent Card. Does **not** change what the
    /// server binds — use [`host`](Self::host) / [`port`](Self::port) for that.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.config.url = url.into();
        self
    }

    /// The host to bind. Defaults to `0.0.0.0`.
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.config.host = host.into();
        self
    }

    /// The port to bind. Defaults to `8000`.
    pub fn port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Forward apcore's own reason for a governance refusal (srs FR-ERR-011).
    pub fn disclose_refusal_reason(mut self, disclose: bool) -> Self {
        self.config.disclose_refusal_reason = disclose;
        self
    }

    /// Bind `host:port` and publish the matching `http://host:port` on the Agent
    /// Card, in one call. The common case for a server with no proxy in front.
    pub fn bind(mut self, host: impl Into<String>, port: u16) -> Self {
        self.config.host = host.into();
        self.config.port = port;
        self.config.url = format!("http://{}:{}", self.config.host, port);
        self
    }

    pub fn build(self) -> APCoreA2A {
        APCoreA2A {
            config: self.config,
        }
    }
}

impl Default for APCoreA2ABuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl APCoreA2A {
    pub fn builder() -> APCoreA2ABuilder {
        APCoreA2ABuilder::new()
    }

    pub fn config(&self) -> &APCoreA2AConfig {
        &self.config
    }
}

/// A [`Discoverer`] that carries its own filesystem roots. `Registry::discover`
/// invokes the discoverer with empty roots ("use defaults"); this wrapper
/// substitutes the roots configured on the [`BackendSource::ExtensionsDir`].
struct RootedDiscoverer {
    inner: apcore::registry::DefaultDiscoverer,
    roots: Vec<String>,
}

#[async_trait::async_trait]
impl apcore::registry::Discoverer for RootedDiscoverer {
    async fn discover(
        &self,
        _roots: &[String],
    ) -> Result<Vec<apcore::registry::DiscoveredModule>, apcore::errors::ModuleError> {
        self.inner.discover(&self.roots).await
    }
}

/// Resolve a [`BackendSource`] into an apcore [`Executor`] and, when available,
/// the shared `Arc<Registry>` (needed to register `sys.*` modules). The pure
/// `Executor` backend owns its registry internally, so no handle is returned.
async fn resolve_backend(
    source: BackendSource,
) -> Result<(Arc<Executor>, Option<Arc<Registry>>), APCoreA2AError> {
    match source {
        BackendSource::ExtensionsDir(path) => {
            let registry = Arc::new(Registry::new());
            let discoverer = RootedDiscoverer {
                inner: apcore::registry::DefaultDiscoverer::new(),
                roots: vec![path.to_string_lossy().to_string()],
            };
            registry
                .discover(&discoverer)
                .await
                .map_err(|e| APCoreA2AError::Server(e.to_string()))?;
            let executor = Arc::new(Executor::new(registry.clone(), Config::default()));
            Ok((executor, Some(registry)))
        }
        BackendSource::Registry(registry) => {
            let executor = Arc::new(Executor::new(registry.clone(), Config::default()));
            Ok((executor, Some(registry)))
        }
        BackendSource::Executor(executor) => Ok((executor, None)),
    }
}

/// Build the A2A ASGI-equivalent app (axum [`Router`]) and Agent Card without
/// starting a server. Useful for embedding and tests.
pub async fn build_app(
    source: BackendSource,
    config: APCoreA2AConfig,
) -> Result<(Router, AgentCard), APCoreA2AError> {
    build_app_with_auth(source, config, None).await
}

/// Like [`build_app`], but applies an [`Authenticator`] (JWT/Bearer auth
/// middleware + agent-card security schemes). Discovery and health stay public.
pub async fn build_app_with_auth(
    source: BackendSource,
    config: APCoreA2AConfig,
    auth: Option<Arc<dyn Authenticator>>,
) -> Result<(Router, AgentCard), APCoreA2AError> {
    // An empty `url` means "derive from host and port", matching Python's
    // documented `url: str | None = None` default. The Agent Card must always
    // publish something resolvable.
    let mut config = config;
    if config.url.trim().is_empty() {
        config.url = format!("http://{}:{}", config.host, config.port);
    }
    let (executor, registry) = resolve_backend(source).await?;
    if executor.registry().list(None, None, None).is_empty() {
        return Err(APCoreA2AError::EmptyRegistry);
    }

    // Observability: structured per-call logging (low overhead), mirroring the
    // Python/TS adapters which install ObsLoggingMiddleware by default.
    let _ = executor.use_middleware(Box::new(apcore::ObsLoggingMiddleware::new(
        apcore::ContextLogger::new("apcore-a2a"),
    )));

    // Optionally register apcore's system.* modules (health/usage/manifest).
    // Requires a shared registry handle, so it is unavailable for the
    // pure-Executor backend.
    //
    // The config is built here rather than passed as `Config::default()`:
    // `register_sys_modules` reads `sys_modules.enabled` and returns an empty
    // context when it is absent, so the default config registered nothing while
    // `let _ =` swallowed the fact that it had (apcore-a2a#5). The result is now
    // inspected, and a failure is reported rather than discarded.
    //
    // `sys_modules.events` has no route in through this binding, so
    // `sys_modules = true` alone registers the six read modules and no
    // `system.control.*` — matching the Python and TypeScript bindings.
    if config.sys_modules {
        match &registry {
            Some(reg) => {
                let mut sys_config = Config::default();
                sys_config.set("sys_modules.enabled", serde_json::json!(true));
                match apcore::register_sys_modules(reg.clone(), &executor, &sys_config, None) {
                    Ok(_) => {
                        let ids: Vec<String> =
                            reg.list(None, Some("system."), None).into_iter().collect();
                        tracing::info!(
                            modules = ?ids,
                            "Registered apcore system modules"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "register_sys_modules failed — continuing without system modules"
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    "sys_modules requested but backend is an Executor; pass a Registry or extensions dir to enable system.* modules"
                );
            }
        }
    }

    warn_on_unprotected_control_surface(&executor);

    let part_converter = PartConverter::new(SchemaConverter::new());
    let agent_executor = Arc::new(
        ApCoreAgentExecutor::new(executor.clone(), part_converter, config.execution_timeout)
            .with_disclose_refusal_reason(config.disclose_refusal_reason),
    );
    let factory = A2AServerFactory::new();
    let mut opts = CreateOptions::new(
        agent_executor,
        &config.name,
        &config.description,
        &config.version,
        &config.url,
    )
    .with_explorer(config.explorer, "/explorer");
    if let Some(authenticator) = auth {
        opts = opts.with_auth(authenticator);
    }
    let (router, card) = factory.create(executor.registry(), opts);
    let router = apply_cors(router, &config.cors_origins);
    Ok((router, card))
}

/// Apply a CORS layer (outermost, so preflight bypasses auth) when origins are
/// configured. No-op for an empty origin list.
fn apply_cors(router: Router, origins: &[String]) -> Router {
    if origins.is_empty() {
        return router;
    }
    use axum::http::{header, HeaderValue, Method};
    use tower_http::cors::CorsLayer;
    let allowed: Vec<HeaderValue> = origins.iter().filter_map(|o| o.parse().ok()).collect();
    let cors = CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);
    router.layer(cors)
}

/// The socket to bind, from `host` and `port`.
///
/// Previously this was string-split out of `config.url` — `url.split("://")
/// .nth(1).unwrap_or("0.0.0.0:8000")` — which conflated the endpoint the Agent
/// Card publishes with the socket the server binds, and failed in three silent
/// ways. A scheme-less value like `127.0.0.1:18999` has no `://`, so the
/// fallback took over and an operator who deliberately typed a loopback address
/// published every skill on **every interface**, on a port they never chose,
/// with nothing logged. A path (`http://127.0.0.1:8080/a2a`) or a missing port
/// (`http://127.0.0.1`) reached `TcpListener::bind` as garbage and failed with
/// an address-parse error naming neither the URL nor the cause.
///
/// `host` and `port` are typed, so none of those are expressible. The error path
/// that remains — a host string that does not resolve — names both values.
pub fn bind_addr(config: &APCoreA2AConfig) -> Result<String, APCoreA2AError> {
    if config.host.trim().is_empty() {
        return Err(APCoreA2AError::Server(
            "config.host is empty; set it to a bind address such as \"127.0.0.1\" or \"0.0.0.0\""
                .to_string(),
        ));
    }
    // An IPv6 literal must be bracketed before being joined to a port.
    let host = if config.host.contains(':') && !config.host.starts_with('[') {
        format!("[{}]", config.host)
    } else {
        config.host.clone()
    };
    Ok(format!("{host}:{}", config.port))
}

/// Warn when the server binds every interface with no authentication.
///
/// The Python (`__main__.py`) and TypeScript (`cli.ts`) bindings have always
/// emitted this; the Rust binding had no equivalent, so the one configuration
/// that most deserves a line in the log produced none.
/// Warn when `system.control.*` is served with nothing gating it (srs FR-AGC-007).
///
/// Reads apcore's `Executor::governance_state` (apcore `PROTOCOL_SPEC` §6.6.5),
/// which answers "is a gate *engaging*" rather than "is an ACL *attached*" — the
/// ACL and approval gates are pipeline *steps*, and the `internal`, `testing` and
/// `minimal` strategies remove them, so an executor can hold an ACL that no step
/// ever consults. This crate has `Executor::acl` right there as a public field
/// and reading it would answer the wrong question.
///
/// Withholding `system.*` from the public card (FR-AGC-003 criterion 12) removes
/// the surface from *discovery*, not from *dispatch*: apcore's approval gate
/// warns once and continues when no `ApprovalHandler` is configured, so the write
/// modules stay callable. This warning exists so the card rule cannot be mistaken
/// for a fix to that. apcore deliberately made the accessor a pure read and left
/// the reaction to the adapter; warning is the reaction.
fn warn_on_unprotected_control_surface(executor: &apcore::Executor) {
    if executor.governance_state().unprotected_control_surface {
        tracing::warn!(
            "apcore system.control.* modules are registered and no built-in governance gate \
             engages for them (no ACL, no ApprovalHandler, or a strategy without the gates). \
             They are withheld from the public Agent Card but remain callable. Configure an \
             acl/ directory, an ApprovalHandler, or ExecutionPolicy with strict = true."
        );
    }
}

fn warn_on_unauthenticated_public_bind(host: &str, has_auth: bool) {
    let loopback = matches!(host, "127.0.0.1" | "::1" | "[::1]" | "localhost");
    if !loopback && !has_auth {
        tracing::warn!(
            host,
            "binding to a non-loopback interface without authentication; \
             consider host=127.0.0.1 or configuring an Authenticator"
        );
    }
}

/// Build the app and serve it (blocking on the current async runtime).
pub async fn async_serve(
    source: BackendSource,
    config: APCoreA2AConfig,
) -> Result<(), APCoreA2AError> {
    async_serve_with_auth(source, config, None).await
}

/// Like [`async_serve`], but with an [`Authenticator`].
pub async fn async_serve_with_auth(
    source: BackendSource,
    config: APCoreA2AConfig,
    auth: Option<Arc<dyn Authenticator>>,
) -> Result<(), APCoreA2AError> {
    let addr = bind_addr(&config)?;
    warn_on_unauthenticated_public_bind(&config.host, auth.is_some());
    let (router, _card) = build_app_with_auth(source, config.clone(), auth).await?;
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        APCoreA2AError::Server(format!(
            "failed to bind {addr} (host={:?}, port={}): {e}",
            config.host, config.port
        ))
    })?;
    axum::serve(listener, router)
        .await
        .map_err(|e| APCoreA2AError::Server(e.to_string()))?;
    Ok(())
}

/// Convenience function to serve modules as an A2A agent (alias for [`async_serve`]).
pub async fn serve(source: BackendSource, config: APCoreA2AConfig) -> Result<(), APCoreA2AError> {
    async_serve(source, config).await
}
