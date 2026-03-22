pub mod github_actions;

use crate::schema::Pipeline;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("failed to parse source file: {0}")]
    Parse(String),

    #[error("unsupported feature: {0}")]
    Unsupported(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Trait for converting from external CI formats to Gauntlet Pipeline JSON.
pub trait Converter {
    fn convert(&self, source: &str) -> Result<Pipeline, ConvertError>;
}
