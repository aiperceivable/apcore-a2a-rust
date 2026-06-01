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
use crate::server::factory::A2AServerFactory;
use crate::storage::{InMemoryTaskStore, TaskStore};

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
    pub url: String,
    pub execution_timeout: u64,
    pub explorer: bool,
    pub metrics: bool,
    /// Register apcore `sys.*` modules (health/usage/manifest). Off by default.
    pub sys_modules: bool,
    /// Allowed CORS origins. Empty = no CORS layer.
    pub cors_origins: Vec<String>,
}

impl Default for APCoreA2AConfig {
    fn default() -> Self {
        Self {
            name: "apcore-a2a".to_string(),
            description: "apcore A2A agent".to_string(),
            version: crate::VERSION.to_string(),
            url: "http://localhost:8000".to_string(),
            execution_timeout: 300,
            explorer: false,
            metrics: false,
            sys_modules: false,
            cors_origins: vec![],
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

    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.config.url = url.into();
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
    let (executor, registry) = resolve_backend(source).await?;
    if executor.registry().list(None, None, None).is_empty() {
        return Err(APCoreA2AError::EmptyRegistry);
    }

    // Observability: structured per-call logging (low overhead), mirroring the
    // Python/TS adapters which install ObsLoggingMiddleware by default.
    let _ = executor.use_middleware(Box::new(apcore::ObsLoggingMiddleware::new(
        apcore::ContextLogger::new("apcore-a2a"),
    )));

    // Optionally register apcore sys.* modules (health/usage/manifest). Requires
    // a shared registry handle, so it is unavailable for the pure-Executor backend.
    if config.sys_modules {
        match &registry {
            Some(reg) => {
                let _ =
                    apcore::register_sys_modules(reg.clone(), &executor, &Config::default(), None);
            }
            None => {
                tracing::warn!(
                    "sys_modules requested but backend is an Executor; pass a Registry or extensions dir to enable sys.* modules"
                );
            }
        }
    }

    let part_converter = PartConverter::new(SchemaConverter::new());
    let agent_executor = Arc::new(ApCoreAgentExecutor::new(
        executor.clone(),
        part_converter,
        config.execution_timeout,
    ));
    let task_store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());

    let factory = A2AServerFactory::new();
    let (router, card) = factory.create(
        executor.registry(),
        agent_executor,
        task_store,
        &config.name,
        &config.description,
        &config.version,
        &config.url,
        auth,
        config.explorer,
        "/explorer",
    );
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
    let (router, _card) = build_app_with_auth(source, config.clone(), auth).await?;
    let addr = config
        .url
        .split("://")
        .nth(1)
        .unwrap_or("0.0.0.0:8000")
        .to_string();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| APCoreA2AError::Server(e.to_string()))?;
    axum::serve(listener, router)
        .await
        .map_err(|e| APCoreA2AError::Server(e.to_string()))?;
    Ok(())
}

/// Convenience function to serve modules as an A2A agent (alias for [`async_serve`]).
pub async fn serve(source: BackendSource, config: APCoreA2AConfig) -> Result<(), APCoreA2AError> {
    async_serve(source, config).await
}
