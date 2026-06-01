//! A2A Client — call remote A2A agents.

pub mod card_fetcher;
#[allow(clippy::module_inception)]
pub mod client;
pub mod exceptions;

pub use card_fetcher::AgentCardFetcher;
pub use client::A2AClient;
pub use exceptions::{A2AClientError, ClientResult};
