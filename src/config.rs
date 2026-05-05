use crate::trigger::TriggerConfig;
use crate::workflow::Step;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,

    /// Trigger configuration — what initiates workflow runs.
    /// Empty vec is allowed at deserialization but rejected by Config::load.
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,

    pub repos: Vec<RepoConfig>,

    /// Steps to execute. Empty vec is allowed at deserialization but rejected by Config::load.
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoConfig {
    pub owner: String,
    pub repo: String,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config from {:?}: {}", path, e))?;
        let config: Self = toml::from_str(&text).map_err(|e| {
            // Include the full TOML deserialization detail in the error message
            // so users see *why* parsing failed (unknown variant, missing field, etc.)
            anyhow::anyhow!("parsing config from {:?}: {}", path, e)
        })?;

        anyhow::ensure!(
            !config.triggers.is_empty(),
            "config {:?} must define at least one [[triggers]] entry",
            path
        );
        anyhow::ensure!(
            !config.steps.is_empty(),
            "config {:?} contains no [[steps]]",
            path
        );
        Ok(config)
    }
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "agent-orchestrator",
    about = "Poll GitHub and run agent workflows"
)]
pub struct Cli {
    #[arg(long, default_value = "config.toml")]
    pub config: std::path::PathBuf,

    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<std::path::PathBuf>,

    /// Print harness agent stdout/stderr to the terminal in addition to
    /// writing them to log files in the data directory.
    #[arg(long)]
    pub show_logs: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn default_data_dir_is_none_when_not_provided() {
        let cli = Cli::try_parse_from(["agent-orchestrator", "--config", "cfg.toml"]).unwrap();
        assert!(
            cli.data_dir.is_none(),
            "expected None when --data-dir not provided, got: {:?}",
            cli.data_dir
        );
    }

    #[test]
    fn custom_data_dir_is_respected() {
        let cli = Cli::try_parse_from([
            "agent-orchestrator",
            "--config",
            "cfg.toml",
            "--data-dir",
            "/tmp/my-data",
        ])
        .unwrap();
        assert_eq!(cli.data_dir.unwrap().to_string_lossy(), "/tmp/my-data");
    }

    #[test]
    fn explicit_trigger_deserializes() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[triggers]]
type = "github_issue_assigned"
assigned_to = "carol"
allowed_users = ["dave"]

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
harness = { type = "hermes", profile = "cto" }
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.triggers.len(), 1);
        match &config.triggers[0] {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_users,
            } => {
                assert_eq!(assigned_to, "carol");
                assert_eq!(allowed_users, &vec!["dave"]);
            }
            TriggerConfig::GithubPrReview { .. } => {
                panic!("expected GithubIssueAssigned, got GithubPrReview");
            }
        }
        // Verify step harness loaded
        match &config.steps[0].harness {
            crate::harness::HarnessConfig::Hermes { profile, .. } => {
                assert_eq!(profile, "cto");
            }
        }
    }

    #[test]
    fn no_triggers_errors() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
harness = { type = "hermes", profile = "cto" }
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("trigger"),
            "expected 'trigger' in error: {msg}"
        );
    }

    #[test]
    fn toml_parse_error_includes_detail() {
        use std::io::Write;
        // Use an unknown hook variant to trigger a TOML deserialization error
        let toml = r#"
poll_interval_secs = 60

[[triggers]]
type = "github_issue_assigned"
assigned_to = "test"
allowed_users = ["test"]

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
harness = { type = "hermes", profile = "cto" }

[[steps.post_hooks]]
type = "file_is_whale"
path = "{{output_path}}"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        // The error should include the TOML detail (e.g. "unknown variant")
        assert!(
            msg.contains("file_not_empty") || msg.contains("unknown variant"),
            "expected TOML error detail in error message, got: {msg}"
        );
    }

    #[test]
    fn both_trigger_types_deserialize() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[triggers]]
type = "github_issue_assigned"
assigned_to = "carol"
allowed_users = ["dave"]

[[triggers]]
type = "github_pr_review"
allowed_users = ["eve"]

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
harness = { type = "hermes", profile = "cto" }
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.triggers.len(), 2);
        match &config.triggers[0] {
            TriggerConfig::GithubIssueAssigned { assigned_to, .. } => {
                assert_eq!(assigned_to, "carol");
            }
            other => panic!("expected GithubIssueAssigned, got {:?}", other),
        }
        match &config.triggers[1] {
            TriggerConfig::GithubPrReview { allowed_users } => {
                assert_eq!(allowed_users, &vec!["eve"]);
            }
            other => panic!("expected GithubPrReview, got {:?}", other),
        }
    }
}
