use crate::workflow::Step;
use anyhow::Context;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub assigned_to: String,
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub allowed_issue_creators: Vec<String>,
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
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing config from {:?}", path))?;
        anyhow::ensure!(
            !config.steps.is_empty(),
            "config {:?} contains no [[steps]]",
            path
        );
        anyhow::ensure!(
            !config.allowed_issue_creators.is_empty(),
            "config {:?} must list at least one allowed_issue_creators entry",
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
            "--config", "cfg.toml",
            "--data-dir", "/tmp/my-data",
        ])
        .unwrap();
        assert_eq!(cli.data_dir.unwrap().to_string_lossy(), "/tmp/my-data");
    }
}
