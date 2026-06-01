//! A2A 1.0 protocol wire types (hand-rolled; there is no Rust A2A SDK).
//!
//! These serde types match the A2A 1.0 wire format documented in the shared
//! `apcore-a2a/docs` spec and verified against the Python (`a2a-sdk`) and
//! TypeScript (`@a2a-js/sdk`) 1.0 serializations:
//!
//! - `Part` is a flattened `oneof`: `{"text": …}` / `{"data": …}` / `{"url": …}`
//!   / `{"raw": …}` — no `type` discriminator.
//! - `TaskState` serializes as its full enum name (`TASK_STATE_SUBMITTED`, …).
//! - `Role` is an enum (`ROLE_USER` / `ROLE_AGENT`).
//! - Stream/push events are a `oneof` keyed `task` / `statusUpdate` /
//!   `artifactUpdate` — there is no `type`/`kind` field and no `final` flag.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A2A 1.0 message sender role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "ROLE_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "ROLE_USER")]
    User,
    #[serde(rename = "ROLE_AGENT")]
    Agent,
}

/// A2A 1.0 task lifecycle state. Serializes as the full protobuf enum name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
}

impl TaskState {
    /// Terminal states that end an SSE stream (A2A 1.0 has no `final` flag).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }
}

/// A2A 1.0 `Part` — a flattened protobuf `oneof` over content cases.
///
/// `#[serde(untagged)]` makes it serialize/deserialize as the bare content
/// object (`{"text": …}` etc.) with no discriminator, matching the wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Part {
    Text { text: String },
    Data { data: Value },
    FileUrl { url: String },
    FileRaw { raw: String },
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Part::Text { text: s.into() }
    }
    pub fn data(v: Value) -> Self {
        Part::Data { data: v }
    }
}

/// A2A 1.0 message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<Value>,
}

impl Message {
    /// Build a single-text agent message (used for status messages).
    pub fn agent_text(text: impl Into<String>) -> Self {
        Message {
            message_id: uuid::Uuid::new_v4().to_string(),
            role: Role::Agent,
            parts: vec![Part::text(text)],
            context_id: None,
            task_id: None,
            metadata: None,
        }
    }
}

/// A2A 1.0 task status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timestamp: Option<String>,
}

impl TaskStatus {
    pub fn new(state: TaskState) -> Self {
        TaskStatus {
            state,
            message: None,
            timestamp: Some(now_iso8601()),
        }
    }

    pub fn with_message(state: TaskState, message: Message) -> Self {
        TaskStatus {
            state,
            message: Some(message),
            timestamp: Some(now_iso8601()),
        }
    }
}

/// A2A 1.0 artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl Artifact {
    pub fn new(artifact_id: impl Into<String>, parts: Vec<Part>) -> Self {
        Artifact {
            artifact_id: artifact_id.into(),
            parts,
            name: None,
        }
    }
}

/// A2A 1.0 task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub history: Vec<Message>,
}

/// A2A 1.0 `TaskStatusUpdateEvent` (no `final` flag; no `kind`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
}

/// A2A 1.0 `TaskArtifactUpdateEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    pub append: bool,
    pub last_chunk: bool,
}

/// A2A 1.0 streaming/push event `oneof`. Externally tagged, so it serializes as
/// `{"task": …}` / `{"statusUpdate": …}` / `{"artifactUpdate": …}` — exactly the
/// wire discriminator (there is no separate `type`/`kind` field).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamEvent {
    Task(Task),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// Current UTC time as an ISO-8601 / RFC-3339 string (millisecond precision).
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn part_text_serializes_flat_without_type() {
        let v = serde_json::to_value(Part::text("hi")).unwrap();
        assert_eq!(v, json!({ "text": "hi" }));
    }

    #[test]
    fn part_data_serializes_flat() {
        let v = serde_json::to_value(Part::data(json!({ "a": 1 }))).unwrap();
        assert_eq!(v, json!({ "data": { "a": 1 } }));
    }

    #[test]
    fn part_deserializes_from_oneof_wire() {
        let t: Part = serde_json::from_value(json!({ "text": "x" })).unwrap();
        assert_eq!(t, Part::text("x"));
        let d: Part = serde_json::from_value(json!({ "data": { "k": true } })).unwrap();
        assert_eq!(d, Part::data(json!({ "k": true })));
    }

    #[test]
    fn task_state_serializes_full_enum_name() {
        assert_eq!(
            serde_json::to_value(TaskState::Completed).unwrap(),
            json!("TASK_STATE_COMPLETED")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Submitted).unwrap(),
            json!("TASK_STATE_SUBMITTED")
        );
    }

    #[test]
    fn role_serializes_enum_name() {
        assert_eq!(
            serde_json::to_value(Role::User).unwrap(),
            json!("ROLE_USER")
        );
        assert_eq!(
            serde_json::to_value(Role::Agent).unwrap(),
            json!("ROLE_AGENT")
        );
    }

    #[test]
    fn terminal_states() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Canceled.is_terminal());
        assert!(!TaskState::Working.is_terminal());
        assert!(!TaskState::Submitted.is_terminal());
    }

    #[test]
    fn stream_event_uses_oneof_wrapper_key() {
        let ev = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus::new(TaskState::Working),
        });
        let v = serde_json::to_value(ev).unwrap();
        // A2A 1.0: discriminated by wrapper key, no `type`/`kind`/`final`.
        assert!(v.get("statusUpdate").is_some());
        assert_eq!(
            v["statusUpdate"]["status"]["state"],
            json!("TASK_STATE_WORKING")
        );
        assert!(v["statusUpdate"].get("final").is_none());

        let art = StreamEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            artifact: Artifact::new("art-1", vec![Part::text("x")]),
            append: false,
            last_chunk: true,
        });
        let av = serde_json::to_value(art).unwrap();
        assert!(av.get("artifactUpdate").is_some());
        assert_eq!(av["artifactUpdate"]["lastChunk"], json!(true));
    }
}
