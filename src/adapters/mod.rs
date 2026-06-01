//! Adapters — translate between apcore and A2A data models.

pub mod agent_card;
pub mod errors;
pub mod parts;
pub mod schema;
pub mod skill_mapper;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("schema conversion failed: {0}")]
    SchemaConversion(String),
    #[error("invalid module ID '{id}'")]
    InvalidModuleId { id: String },
}

pub use agent_card::AgentCardBuilder;
pub use errors::{register_a2a_error_formatter, A2aErrorFormatter, ErrorMapper};
pub use parts::PartConverter;
pub use schema::SchemaConverter;
pub use skill_mapper::SkillMapper;
