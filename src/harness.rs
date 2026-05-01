//! Pluggable agent harness system.
//!
//! A harness is the agent backend that executes a workflow step.
//! Each variant carries its own harness-specific configuration.
//! Currently only Hermes is supported, but this trait allows
//! adding cursor, claude-code, etc. in the future.

use crate::workflow::Step;
use anyhow::Result;
use std::path::Path;

/// Config-side harness definition deserialized from TOML.
///
/// Each variant carries harness-specific options. For example,
/// `Hermes` has `profile`, `worktree`, `provider`, and `model`,
/// because those are hermes CLI flags — not generic step concerns.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessConfig {
    /// Invoke the hermes CLI agent.
    ///
    /// ```toml
    /// harness = { type = "hermes", profile = "cto", worktree = true, provider = "openai", model = "o3" }
    /// ```
    Hermes {
        /// Required: hermes profile name passed via `--profile <name>`.
        profile: String,
        /// When true, passes `--worktree` to hermes.
        #[serde(default)]
        worktree: bool,
        /// Optional provider passed to hermes via `--provider <provider>`.
        #[serde(default)]
        provider: Option<String>,
        /// Optional model passed to hermes via `--model <model>`.
        #[serde(default)]
        model: Option<String>,
    },
    // Future variants:
    // Cursor { binary: String, prompt: String },
    // ClaudeCode { binary: String, prompt: String },
}

/// Runtime harness trait — each agent backend implements this.
///
/// The trait uses `'static` return lifetime because all data needed
/// by the future is owned or cloned into the async block.
pub trait Harness {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Execute a single workflow step.
    ///
    /// `issue` is a human-readable identifier like "owner/repo#123"
    /// for log context.
    fn run_step(
        &self,
        step: &Step,
        workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>>;
}

/// Build a runtime Harness from its config.
impl HarnessConfig {
    pub fn build(&self) -> Box<dyn Harness + Send> {
        match self {
            HarnessConfig::Hermes {
                profile,
                worktree,
                provider,
                model,
            } => Box::new(crate::hermes::HermesHarness {
                profile: profile.clone(),
                worktree: *worktree,
                provider: provider.clone(),
                model: model.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_config_deserializes() {
        let toml = r#"
type = "hermes"
profile = "cto"
worktree = true
provider = "openai"
model = "o3"
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::Hermes {
                profile,
                worktree,
                provider,
                model,
            } => {
                assert_eq!(profile, "cto");
                assert!(worktree);
                assert_eq!(provider, Some("openai".to_string()));
                assert_eq!(model, Some("o3".to_string()));
            }
        }
    }

    #[test]
    fn hermes_config_minimal() {
        let toml = r#"
type = "hermes"
profile = "cto"
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::Hermes {
                profile,
                worktree,
                provider,
                model,
            } => {
                assert_eq!(profile, "cto");
                assert!(!worktree);
                assert!(provider.is_none());
                assert!(model.is_none());
            }
        }
    }

    #[test]
    fn build_hermes() {
        let config = HarnessConfig::Hermes {
            profile: "cto".to_string(),
            worktree: false,
            provider: None,
            model: None,
        };
        let harness = config.build();
        assert_eq!(harness.name(), "hermes");
    }
}
