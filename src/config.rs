use crate::trigger::TriggerConfig;
use crate::workflow::Step;

/// Git configuration group.
///
/// Controls how the orchestrator manages the repository clone and worktrees.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// Branch that worktrees are based on and the repo is checked out on.
    /// Defaults to "main" if not specified.
    #[serde(default = "default_branch")]
    pub default_branch: String,

    /// When true (default), the orchestrator clones/pulls the repo before
    /// running workflow steps. When false, no git operations are performed.
    #[serde(default = "default_true")]
    pub clone: bool,

    /// When true, a fresh git worktree is created for each workflow run and
    /// removed on completion. Requires `clone = true`.
    #[serde(default)]
    pub worktree: bool,
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            default_branch: default_branch(),
            clone: true,
            worktree: false,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Trigger configuration — what initiates workflow runs.
    /// Empty vec is allowed at deserialization but rejected by Config::load.
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,

    pub repos: Vec<RepoConfig>,

    /// Git configuration for repo and worktree management.
    #[serde(default)]
    pub git: GitConfig,

    /// Steps to execute. Empty vec is allowed at deserialization but rejected by Config::load.
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
        anyhow::ensure!(
            !config.git.worktree || config.git.clone,
            "config {:?}: git.worktree requires git.clone to be true",
            path
        );
        Ok(config)
    }

    /// Load all .toml files from a directory, sorted by filename for determinism.
    /// Returns an error if the directory contains no .toml files or any file
    /// fails validation.
    pub fn load_all(dir: &std::path::Path) -> anyhow::Result<Vec<Self>> {
        anyhow::ensure!(
            dir.is_dir(),
            "--workflows path {:?} is not a directory",
            dir
        );

        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort();

        anyhow::ensure!(
            !entries.is_empty(),
            "no .toml files found in --workflows directory {:?}",
            dir
        );

        let mut configs = Vec::with_capacity(entries.len());
        for path in &entries {
            tracing::info!("loading workflow config: {}", path.display());
            configs.push(Self::load(path)?);
        }
        Ok(configs)
    }
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "agent-orchestrator",
    about = "Poll GitHub and run agent workflows"
)]
pub struct Cli {
    /// Directory containing workflow TOML files.
    #[arg(long, default_value = ".")]
    pub workflows: std::path::PathBuf,

    /// Maximum number of concurrent workflow runs. 0 means unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Poll interval in seconds. How often to check for new events.
    #[arg(long, default_value_t = 60)]
    pub interval: u64,

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
        let cli = Cli::try_parse_from(["agent-orchestrator", "--workflows", "."]).unwrap();
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
            "--workflows",
            ".",
            "--data-dir",
            "/tmp/my-data",
        ])
        .unwrap();
        assert_eq!(cli.data_dir.unwrap().to_string_lossy(), "/tmp/my-data");
    }

    #[test]
    fn limit_defaults_to_zero() {
        let cli = Cli::try_parse_from(["agent-orchestrator", "--workflows", "."]).unwrap();
        assert_eq!(cli.limit, 0, "default limit should be 0 (unlimited)");
    }

    #[test]
    fn explicit_limit_is_respected() {
        let cli = Cli::try_parse_from(["agent-orchestrator", "--workflows", ".", "--limit", "4"])
            .unwrap();
        assert_eq!(cli.limit, 4);
    }

    #[test]
    fn interval_defaults_to_60() {
        let cli = Cli::try_parse_from(["agent-orchestrator", "--workflows", "."]).unwrap();
        assert_eq!(cli.interval, 60, "default interval should be 60 seconds");
    }

    #[test]
    fn explicit_interval_is_respected() {
        let cli =
            Cli::try_parse_from(["agent-orchestrator", "--workflows", ".", "--interval", "30"])
                .unwrap();
        assert_eq!(cli.interval, 30);
    }

    #[test]
    fn explicit_trigger_deserializes() {
        use std::io::Write;
        let toml = r#"

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
            TriggerConfig::LocalFile { .. } => {
                panic!("expected GithubIssueAssigned, got LocalFile");
            }
        }
        // Verify step harness loaded
        match &config.steps[0].harness {
            crate::harness::HarnessConfig::Hermes { profile, .. } => {
                assert_eq!(profile, "cto");
            }
            _ => panic!("expected Hermes harness"),
        }
    }

    #[test]
    fn no_triggers_errors() {
        use std::io::Write;
        let toml = r#"

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
    fn git_config_defaults() {
        use std::io::Write;
        let toml = r#"

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
        assert_eq!(config.git.default_branch, "main");
        assert!(config.git.clone);
        assert!(!config.git.worktree);
    }

    #[test]
    fn git_worktree_requires_clone() {
        use std::io::Write;
        let toml = r#"

[git]
clone = false
worktree = true

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
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("worktree requires git.clone"),
            "expected worktree/requires/clone error, got: {msg}"
        );
    }

    #[test]
    fn both_trigger_types_deserialize() {
        use std::io::Write;
        let toml = r#"

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

    #[test]
    fn load_all_discovers_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        let toml_content = r#"

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
        // Write two .toml files
        std::fs::write(dir.path().join("alpha.toml"), toml_content).unwrap();
        std::fs::write(dir.path().join("beta.toml"), toml_content).unwrap();
        // Write a non-toml file that should be ignored
        std::fs::write(dir.path().join("readme.md"), "ignore me").unwrap();

        let configs = Config::load_all(dir.path()).unwrap();
        assert_eq!(
            configs.len(),
            2,
            "expected 2 configs, got {}",
            configs.len()
        );
    }

    #[test]
    fn load_all_rejects_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load_all(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no .toml files"),
            "expected 'no .toml files' in error, got: {msg}"
        );
    }

    #[test]
    fn load_all_rejects_non_directory() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let err = Config::load_all(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a directory"),
            "expected 'not a directory' in error, got: {msg}"
        );
    }

    #[test]
    fn config_rejects_unknown_fields() {
        use std::io::Write;
        let toml = r#"

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

typo_field = "oops"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
    }

    #[test]
    fn git_config_rejects_unknown_fields() {
        use std::io::Write;
        let toml = r#"

[git]
default_branch = "main"
typo = true

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
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
    }

    #[test]
    fn repo_config_rejects_unknown_fields() {
        use std::io::Write;
        let toml = r#"

[[triggers]]
type = "github_issue_assigned"
assigned_to = "carol"
allowed_users = ["dave"]

[[repos]]
owner = "o"
repo = "r"
typo = "oops"

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
            msg.contains("unknown field"),
            "expected 'unknown field' in error, got: {msg}"
        );
    }
}
