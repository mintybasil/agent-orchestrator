//! Pluggable agent harness system.
//!
//! A harness is the agent backend that executes a workflow step.
//! Currently only Hermes is supported, but this trait allows
//! adding cursor, claude-code, etc. in the future.

use crate::workflow::Step;
use anyhow::Result;
use std::path::Path;

/// Config-side harness definition deserialized from TOML.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessConfig {
    #[default]
    Hermes,
    // Future variants:
    // Cursor { binary: String },
    // ClaudeCode { binary: String },
}

/// Runtime harness trait — each agent backend implements this.
///
/// The trait uses `'static` return lifetime because all data needed
/// by the future is owned or cloned into the async block.
pub trait Harness {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Execute a single workflow step.
    fn run_step(
        &self,
        step: &Step,
        workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>>;
}

/// Build a runtime Harness from its config.
impl HarnessConfig {
    pub fn build(&self) -> Box<dyn Harness + Send> {
        match self {
            HarnessConfig::Hermes => Box::new(crate::hermes::HermesHarness),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_config_deserializes() {
        let toml = r#"type = "hermes""#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        assert!(matches!(config, HarnessConfig::Hermes));
    }

    #[test]
    fn default_is_hermes() {
        assert!(matches!(HarnessConfig::default(), HarnessConfig::Hermes));
    }

    #[test]
    fn build_hermes() {
        let harness = HarnessConfig::Hermes.build();
        assert_eq!(harness.name(), "hermes");
    }
}