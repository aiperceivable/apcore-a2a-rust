//! Self-contained runnable example: expose an apcore module as an A2A agent.
//!
//! This example registers a tiny in-code `demo.greet` module and serves it, so
//! it works out of the box with **no external setup** — no extensions directory,
//! no deployed modules.
//!
//! Run it with:
//!
//! ```bash
//! # Serve the demo module on port 8000
//! cargo run --example run
//!
//! # Bind to a different URL/port if 8000 is taken (e.g. by a Docker container)
//! A2A_URL=http://localhost:8001 cargo run --example run
//! ```
//!
//! Once running, the agent is reachable at (default port 8000):
//!   - Agent Card:  http://localhost:8000/.well-known/agent-card.json  (0.3 alias: /.well-known/agent.json)
//!   - Health:      http://localhost:8000/health
//!   - Explorer UI: http://localhost:8000/explorer  (open in a browser)
//!
//! Probe it from another terminal:
//!
//! ```bash
//! curl http://localhost:8000/.well-known/agent-card.json     # Agent Card (A2A 1.0)
//! curl http://localhost:8000/health                          # Health check
//! # Invoke the skill over JSON-RPC (A2A message/send). The skill's inputs are
//! # passed as a `data` part; `metadata.skillId` selects the module:
//! curl -X POST http://localhost:8000/ \
//!   -H 'content-type: application/json' \
//!   -d '{"jsonrpc":"2.0","id":"1","method":"message/send",
//!        "params":{"message":{"messageId":"m1","role":"ROLE_USER",
//!                             "parts":[{"data":{"name":"Tercel"}}]},
//!                  "metadata":{"skillId":"demo.greet"}}}'
//! # => artifacts[0].parts[0].data == {"greeting":"Hello, Tercel!"}
//! ```
//!
//! To serve your own modules from a registry instead, build an
//! `apcore::registry::Registry`, register your modules, and pass
//! `BackendSource::Registry(Arc::new(registry))` to `serve`.

use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::{APCoreA2AConfig, APCoreA2AError, BackendSource};
use async_trait::async_trait;
use serde_json::{json, Value};

/// A tiny demo module: greets the `name` it is given.
struct GreetModule;

#[async_trait]
impl Module for GreetModule {
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        })
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "greeting": { "type": "string" } }
        })
    }

    fn description(&self) -> &str {
        "Greets the provided name"
    }

    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        let name = inputs.get("name").and_then(Value::as_str).unwrap_or("world");
        Ok(json!({ "greeting": format!("Hello, {name}!") }))
    }
}

/// Build a registry holding the single `demo.greet` module.
fn demo_registry() -> Arc<Registry> {
    let registry = Registry::new();
    registry
        .register_module("demo.greet", Box::new(GreetModule))
        .expect("register demo.greet module");
    Arc::new(registry)
}

#[tokio::main]
async fn main() {
    let source = BackendSource::Registry(demo_registry());

    // Bind URL is overridable via A2A_URL so the example can dodge a port
    // already taken by another server (e.g. a Docker container on :8000).
    let url = std::env::var("A2A_URL").unwrap_or_else(|_| "http://localhost:8000".to_string());

    let config = APCoreA2AConfig {
        name: "example-agent".to_string(),
        description: "apcore-a2a runnable example".to_string(),
        explorer: true,
        url,
        ..Default::default()
    };

    println!("Serving demo.greet on {} ...", config.url);
    println!("  Agent Card:  {}/.well-known/agent-card.json", config.url);
    println!("  Health:      {}/health", config.url);
    println!("  Explorer UI: {}/explorer", config.url);

    match apcore_a2a::serve(source, config).await {
        Ok(()) => {}
        Err(APCoreA2AError::EmptyRegistry) => {
            // Unreachable here (the registry always has demo.greet), but kept so
            // the example stays correct if you swap in your own backend source.
            eprintln!("\nNo modules found in the registry.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\nFailed to start A2A server: {e}");
            let msg = e.to_string();
            if msg.contains("in use") || msg.contains("Address already") {
                eprintln!(
                    "The port is already taken by another process. \
                     Free it, or bind elsewhere:\n\
                     \n    A2A_URL=http://localhost:8001 cargo run --example run\n"
                );
            }
            std::process::exit(1);
        }
    }
}
