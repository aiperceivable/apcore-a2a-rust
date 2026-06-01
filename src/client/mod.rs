//! A2A Client — call remote A2A agents.

pub mod card_fetcher;
#[allow(clippy::module_inception)]
pub mod client;

pub use card_fetcher::AgentCardFetcher;
pub use client::A2AClient;
