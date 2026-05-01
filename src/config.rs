use crate::trigger::TriggerConfig;
use crate::workflow::Step;
use anyhow::Context;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,

    /// New-style trigger configuration.
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,

    /// Backward-compat: if triggers is empty, these legacy fields are used to
    /// build a GithubIssueAssigned trigger automatically.
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub allowed_issue_creators: Option<Vec<String>>,

    pub repos: Vec<RepoConfig>,

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
            .with_context(|| format!("reading config from {:?}", path))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing config from {:?}", path))?;

        // Backward compatibility: if no explicit [[triggers]] but legacy fields
        // are present, synthesize a GithubIssueAssigned trigger.
        if config.triggers.is_empty()
            && let Some(ref assigned_to) = config.assigned_to
        {
                let creators = config
                    .allowed_issue_creators
                    .clone()
                    .unwrap_or_else(|| vec![assigned_to.clone()]);
                config.triggers.push(TriggerConfig::GithubIssueAssigned {
                    assigned_to: assigned_to.clone(),
                    allowed_issue_creators: creators,
                });
            }

        anyhow::ensure!(
            !config.triggers.is_empty(),
            "config {:?} must define at least one [[triggers]] entry or set assigned_to",
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
    fn backward_compat_assigned_to_builds_trigger() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60
assigned_to = "alice"
allowed_issue_creators = ["bob"]

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
profile = "cto"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.triggers.len(), 1);
        match &config.triggers[0] {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_issue_creators,
            } => {
                assert_eq!(assigned_to, "alice");
                assert_eq!(allowed_issue_creators, &vec!["bob"]);
            }
        }
    }

    #[test]
    fn explicit_triggers_override_legacy_fields() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[triggers]]
type = "github_issue_assigned"
assigned_to = "carol"
allowed_issue_creators = ["dave"]

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
profile = "cto"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.triggers.len(), 1);
        match &config.triggers[0] {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_issue_creators,
            } => {
                assert_eq!(assigned_to, "carol");
                assert_eq!(allowed_issue_creators, &vec!["dave"]);
            }
        }
    }

    #[test]
    fn no_triggers_no_assigned_to_errors() {
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[repos]]
owner = "o"
repo = "r"

[[steps]]
name = "test"
prompt_template = "do thing"
profile = "cto"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("triggers"));
    }
}