<div align="center">
  <img src="https://raw.githubusercontent.com/aiperceivable/apcore-a2a/main/apcore-a2a-logo.svg" alt="apcore-a2a logo" width="200"/>
</div>

# apcore-a2a (Rust)

[![Crates.io](https://img.shields.io/crates/v/apcore-a2a)](https://crates.io/crates/apcore-a2a)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## What is apcore-a2a?

**apcore-a2a** is the [A2A (Agent-to-Agent)](https://google.github.io/A2A/) protocol adapter for the [apcore](https://github.com/aiperceivable/apcore-rust) ecosystem.

It solves a common problem: **you've built AI capabilities with apcore modules, but you need them to talk to other AI agents over a standard protocol.** apcore-a2a bridges that gap — it reads your existing module metadata (schemas, descriptions, examples) and automatically exposes them as a standards-compliant A2A server. No hand-written Agent Cards, no JSON-RPC boilerplate, no manual task lifecycle management.

**In short:** `apcore modules` + `apcore-a2a` = a fully functional A2A agent, ready to be discovered and invoked by any A2A-compatible client.

Built on [axum](https://github.com/tokio-rs/axum) and [tokio](https://tokio.rs/).

> **Also available in:** [Python](https://github.com/aiperceivable/apcore-a2a-python) | [TypeScript](https://github.com/aiperceivable/apcore-a2a-typescript)

## Features

- **One-call server** — launch a compliant A2A server with `serve(source, config)`
- **Automatic Agent Card** — `/.well-known/agent-card.json` (A2A 1.0; `/.well-known/agent.json` kept as a 0.3 alias) generated from module metadata
- **Skill mapping** — apcore modules become A2A Skills with names, descriptions, tags, and examples; `metadata.display.a2a` overrides surface-facing fields (§5.13)
- **Full task lifecycle** — submitted, working, completed, failed, canceled, input-required
- **JWT authentication** — tokens bridged to apcore's Identity context
- **Built-in client** — `A2AClient` for calling remote A2A agents
- **CLI support** — `apcore-a2a serve` for zero-code startup
- **Pluggable storage** — `TaskStore` trait for custom backends
- **Observability** — `/health` endpoint
- **Config Bus** — registers `apcore-a2a` namespace with `APCORE_A2A` env prefix (apcore 0.15.1)
- **Error Formatter Registry** — registers A2A error formatter with apcore ecosystem (§8.8)

## Requirements

- Rust edition 2021
- `apcore` 0.22
- `apcore-toolkit` 0.8

---

## Getting Started

### Add to your project

```toml
[dependencies]
apcore-a2a = "0.4"
```

### Expose your modules as an A2A Agent

```rust
use apcore_a2a::{APCoreA2A, APCoreA2AConfig, BackendSource};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let source = BackendSource::ExtensionsDir(PathBuf::from("./extensions"));
    let config = APCoreA2AConfig::default();

    apcore_a2a::serve(source, config).await.unwrap();
}
```

### Call a remote A2A Agent

```rust
use apcore_a2a::A2AClient;

#[tokio::main]
async fn main() {
    let client = A2AClient::new("http://remote-agent:8000");

    let task = client
        .send_message("my.skill", "Hello from Rust!")
        .await
        .unwrap();

    println!("Result: {}", task);
}
```

### Add authentication

```rust
use apcore_a2a::{JWTAuthenticator, ClaimMapping};

let auth = JWTAuthenticator::new("your-secret-key")
    .with_claim_mapping(ClaimMapping {
        id_claim: "sub".to_string(),
        roles_claim: "roles".to_string(),
    });
```

---

## Architecture

| A2A Concept    | apcore Mapping                            |
| -------------- | ----------------------------------------- |
| **Agent Card** | Derived from Registry configuration       |
| **Skill id**   | `module_id`                               |
| **Skill name** | `metadata.display.a2a.alias` or humanized `module_id` |
| **Skill desc** | `metadata.display.a2a.description` or `module.description` |
| **Skill tags** | `metadata.display.tags` or `module.tags`  |
| **Task**       | Managed execution of `Executor.call_async()` |
| **Security**   | Bridged to apcore's `Identity` context    |

### Module Structure

```
src/
  adapters/    AgentCardBuilder, SkillMapper, SchemaConverter, ErrorMapper, PartConverter
  auth/        JWTAuthenticator, AuthMiddleware, Authenticator trait
  server/      A2AServerFactory, ApCoreAgentExecutor
  client/      A2AClient, AgentCardFetcher
  storage/     TaskStore trait, InMemoryTaskStore
  explorer/    (planned)
  apcore_a2a.rs  APCoreA2A builder, serve(), async_serve()
  cli.rs       CLI entrypoint
```

## Examples

A self-contained example lives in [`examples/run/main.rs`](examples/run/main.rs). It
registers a tiny in-code `demo.greet` module and serves it as an A2A agent — **no
extensions directory or deployed modules required**:

```bash
# Serve the demo module on port 8000
cargo run --example run

# Bind to a different port if 8000 is taken (e.g. by a Docker container)
A2A_URL=http://localhost:8001 cargo run --example run
```

> **Port 8000 already in use?** If `curl http://localhost:8000/...` returns a
> non-apcore response such as `{"detail":"Not Found"}` with a `server: uvicorn`
> header, another process (often a Docker container) owns the port. Find it with
> `lsof -nP -iTCP:8000 -sTCP:LISTEN`, then either stop it or set `A2A_URL` to a
> free port as shown above.

Once it's running, probe the agent from another terminal:

```bash
curl http://localhost:8000/.well-known/agent-card.json   # Agent Card (A2A 1.0; lists demo.greet)
# (0.3 alias) curl http://localhost:8000/.well-known/agent.json
curl http://localhost:8000/health                   # Health check
open  http://localhost:8000/explorer                # Explorer UI (browser)

# Invoke the skill — inputs go in a `data` part; metadata.skillId picks the module:
curl -X POST http://localhost:8000/ -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"message/send",
       "params":{"message":{"messageId":"m1","role":"ROLE_USER",
                            "parts":[{"data":{"name":"Tercel"}}]},
                 "metadata":{"skillId":"demo.greet"}}}'
# => artifacts[0].parts[0].data == {"greeting":"Hello, Tercel!"}
```

To serve **your own** modules instead, build an `apcore::registry::Registry`,
register your modules, and pass `BackendSource::Registry(Arc::new(registry))` to
`serve` — or point a `BackendSource::ExtensionsDir` at a directory of deployed
modules. See the [`serve`](src/apcore_a2a.rs) entry point for all backend sources.

To verify the example compiles as part of a check, build all examples:

```bash
cargo build --examples
```

### Contributing

```bash
git clone https://github.com/aiperceivable/apcore-a2a-rust.git
cd apcore-a2a-rust
cargo test            # run the test suite
cargo build --examples  # ensure examples still compile
```

## Documentation

- [Product Requirements (PRD)](https://github.com/aiperceivable/apcore-a2a/blob/main/docs/apcore-a2a/prd.md)
- [Technical Design](https://github.com/aiperceivable/apcore-a2a/blob/main/docs/apcore-a2a/tech-design.md)
- [Software Requirements (SRS)](https://github.com/aiperceivable/apcore-a2a/blob/main/docs/apcore-a2a/srs.md)

## License

Apache 2.0 — see [LICENSE](LICENSE).
