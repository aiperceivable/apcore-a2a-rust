//! Explorer — interactive browser UI for testing A2A agents.
//!
//! Enabled via `APCoreA2AConfig::explorer = true`. When on, the server mounts
//! the embedded single-page UI at `/explorer` and serves the enriched agent
//! card (with per-skill `_inputSchemas`) at `/explorer/agent-card`. The UI HTML
//! is embedded at compile time (`include_str!`) and the routes are wired by
//! [`crate::server::factory::A2AServerFactory::create`]; the request handlers
//! live in [`crate::server::handlers`] (`explorer_html` / `explorer_card`).
