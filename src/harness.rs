//! Pluggable agent harness system.
//!
//! A harness is the agent backend that executes a workflow step.
//! Each variant carries its own harness-specific configuration.
//! Currently only Hermes is supported, but this trait allows
//! adding cursor, claude-code, etc. in the future.

use crate::workflow::Step;
use anyhow::Result;
use std::path::Path;

/// Log output configuration passed to harness implementations.
///
/// Groups the log file path and display preference so the `Harness::run_step`
/// signature stays within clippy's argument limit.
pub struct LogConfig {
    /// Path to the log file where stdout and stderr will be written.
    pub log_path: std::path::PathBuf,
    /// When true, harness output is also printed to the terminal via tracing.
    pub show_logs: bool,
}

/// Config-side harness definition deserialized from TOML.
///
/// Each variant carries harness-specific options. For example,
/// `Hermes` has `profile`, `provider`, and `model`,
/// because those are hermes CLI flags — not generic step concerns.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessConfig {
    /// Invoke the hermes CLI agent.
    ///
    /// ```toml
    /// harness = { type = "hermes", profile = "cto", provider = "openai", model = "o3" }
    /// ```
    Hermes {
        /// Required: hermes profile name passed via `--profile <name>`.
        profile: String,
        /// Optional provider passed to hermes via `--provider <provider>`.
        #[serde(default)]
        provider: Option<String>,
        /// Optional model passed to hermes via `--model <model>`.
        #[serde(default)]
        model: Option<String>,
        /// Optional max turns passed to hermes via `--max-turns <n>`.
        #[serde(default)]
        max_turns: Option<u32>,
    },
    /// Invoke the Hermes Agent via its REST API instead of the CLI.
    ///
    /// ```toml
    /// harness = { type = "hermes_api", url = "http://localhost:8000/v1/chat/completions" }
    /// ```
    HermesApi {
        /// Required: The endpoint URL for the Hermes API server
        /// (e.g. "http://localhost:8000/v1/chat/completions").
        url: String,
        /// Optional provider override.
        #[serde(default)]
        provider: Option<String>,
        /// Optional model override.
        #[serde(default)]
        model: Option<String>,
        /// Optional max turns.
        #[serde(default)]
        max_turns: Option<u32>,
    },
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
        log_config: &LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>>;
}

/// Build a runtime Harness from its config.
impl HarnessConfig {
    pub fn build(&self) -> Box<dyn Harness + Send> {
        match self {
            HarnessConfig::Hermes {
                profile,
                provider,
                model,
                max_turns,
            } => Box::new(crate::hermes::HermesHarness {
                profile: profile.clone(),
                provider: provider.clone(),
                model: model.clone(),
                max_turns: *max_turns,
            }),
            HarnessConfig::HermesApi {
                url,
                provider,
                model,
                max_turns,
            } => Box::new(crate::hermes_api::HermesApiHarness {
                url: url.clone(),
                provider: provider.clone(),
                model: model.clone(),
                max_turns: *max_turns,
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
provider = "openai"
model = "o3"
max_turns = 10
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::Hermes {
                profile,
                provider,
                model,
                max_turns,
            } => {
                assert_eq!(profile, "cto");
                assert_eq!(provider, Some("openai".to_string()));
                assert_eq!(model, Some("o3".to_string()));
                assert_eq!(max_turns, Some(10));
            }
            _ => panic!("expected Hermes variant"),
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
                provider,
                model,
                max_turns,
            } => {
                assert_eq!(profile, "cto");
                assert!(provider.is_none());
                assert!(model.is_none());
                assert!(max_turns.is_none());
            }
            _ => panic!("expected Hermes variant"),
        }
    }

    #[test]
    fn build_hermes() {
        let config = HarnessConfig::Hermes {
            profile: "cto".to_string(),
            provider: None,
            model: None,
            max_turns: None,
        };
        let harness = config.build();
        assert_eq!(harness.name(), "hermes");
    }

    #[test]
    fn hermes_config_rejects_unknown_fields() {
        // Ensures that misspelled or misplaced fields like `worktree` in the
        // harness config produce a clear error instead of being silently ignored.
        let toml = r#"
type = "hermes"
profile = "cto"
worktree = true
"#;
        let result = toml::from_str::<HarnessConfig>(toml);
        assert!(
            result.is_err(),
            "expected unknown field 'worktree' to be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field"),
            "error should mention 'unknown field', got: {err}"
        );
        assert!(
            err.contains("worktree"),
            "error should mention the unknown field name, got: {err}"
        );
    }

    #[test]
    fn hermes_api_config_deserializes() {
        let toml = r#"
type = "hermes_api"
url = "http://localhost:8000/v1/chat/completions"
provider = "openai"
model = "o3"
max_turns = 10
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::HermesApi {
                url,
                provider,
                model,
                max_turns,
            } => {
                assert_eq!(url, "http://localhost:8000/v1/chat/completions");
                assert_eq!(provider, Some("openai".to_string()));
                assert_eq!(model, Some("o3".to_string()));
                assert_eq!(max_turns, Some(10));
            }
            other => panic!("expected HermesApi, got {:?}", other),
        }
    }

    #[test]
    fn hermes_api_config_minimal() {
        let toml = r#"
type = "hermes_api"
url = "https://api.example.com/v1/chat/completions"
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::HermesApi {
                url,
                provider,
                model,
                max_turns,
            } => {
                assert_eq!(url, "https://api.example.com/v1/chat/completions");
                assert!(provider.is_none());
                assert!(model.is_none());
                assert!(max_turns.is_none());
            }
            other => panic!("expected HermesApi, got {:?}", other),
        }
    }

    #[test]
    fn hermes_api_config_rejects_unknown_fields() {
        let toml = r#"
type = "hermes_api"
url = "http://localhost:8000/v1/chat/completions"
profile = "cto"
"#;
        let result = toml::from_str::<HarnessConfig>(toml);
        assert!(
            result.is_err(),
            "expected unknown field 'profile' to be rejected for hermes_api"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field"),
            "error should mention 'unknown field', got: {err}"
        );
    }

    #[test]
    fn build_hermes_api() {
        let config = HarnessConfig::HermesApi {
            url: "http://localhost:8000/v1/chat/completions".to_string(),
            provider: None,
            model: None,
            max_turns: None,
        };
        let harness = config.build();
        assert_eq!(harness.name(), "hermes_api");
    }
}
