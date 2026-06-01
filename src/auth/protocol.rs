//! Authenticator trait for A2A auth.

use apcore::context::Identity;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

#[async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, headers: &HashMap<String, String>) -> Option<Identity>;
    fn security_schemes(&self) -> Option<Value>;
}
