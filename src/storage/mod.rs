//! Task storage abstraction for A2A tasks.
//!
//! Provides the TaskStore trait and an in-memory implementation.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn save(&self, task_id: &str, task: Value) -> Result<(), String>;
    async fn get(&self, task_id: &str) -> Result<Option<Value>, String>;
    async fn delete(&self, task_id: &str) -> Result<(), String>;
    /// Return all stored tasks.
    async fn list(&self) -> Result<Vec<Value>, String>;
}

pub struct InMemoryTaskStore {
    store: Mutex<HashMap<String, Value>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn save(&self, task_id: &str, task: Value) -> Result<(), String> {
        self.store
            .lock()
            .map_err(|e| e.to_string())?
            .insert(task_id.to_string(), task);
        Ok(())
    }

    async fn get(&self, task_id: &str) -> Result<Option<Value>, String> {
        Ok(self
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .get(task_id)
            .cloned())
    }

    async fn delete(&self, task_id: &str) -> Result<(), String> {
        self.store
            .lock()
            .map_err(|e| e.to_string())?
            .remove(task_id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Value>, String> {
        Ok(self
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .values()
            .cloned()
            .collect())
    }
}
