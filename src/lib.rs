//! # apcore-a2a
//!
//! A2A protocol adapter for apcore — expose apcore modules as A2A agents.

pub mod adapters;
pub mod apcore_a2a;
pub mod auth;
pub mod cli;
pub mod client;
pub mod explorer;
pub mod server;
pub mod storage;
pub mod types;

/// Crate version, taken from `Cargo.toml` at compile time.
///
/// Derived rather than hand-maintained: a literal drifts from the packaged
/// version on every release and is then reported on the Agent Card and by
/// `apcore-a2a --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Re-exports: core bridge
pub use crate::apcore_a2a::{
    async_serve, async_serve_with_auth, build_app, build_app_with_auth, serve,
};
pub use crate::apcore_a2a::{
    APCoreA2A, APCoreA2ABuilder, APCoreA2AConfig, APCoreA2AError, BackendSource,
};

// Re-exports: auth
pub use crate::auth::jwt::{ClaimMapping, JWTAuthenticator};
pub use crate::auth::middleware::{AuthMiddlewareLayer, AuthMiddlewareService, AUTH_IDENTITY};
pub use crate::auth::protocol::Authenticator;

// Re-exports: server
pub use crate::server::executor::ApCoreAgentExecutor;
pub use crate::server::factory::A2AServerFactory;

// Re-exports: adapters
pub use crate::adapters::{
    register_a2a_error_formatter, A2aErrorFormatter, AdapterError, AgentCardBuilder, ErrorMapper,
    PartConverter, SchemaConverter, SkillMapper,
};

// Re-exports: client
pub use crate::client::{A2AClient, A2AClientError, AgentCardFetcher, ClientResult};
