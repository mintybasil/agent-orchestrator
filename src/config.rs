use anyhow::Context;
use crate::workflow::Step;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub assigned_to: String,
    pub repos: Vec<RepoConfig>,
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
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("parsing config from {:?}", path))?;
        anyhow::ensure!(!config.steps.is_empty(), "config {:?} contains no [[steps]]", path);
        Ok(config)
    }
}

#[derive(clap::Parser, Debug)]
#[command(name = "agent-orchestrator", about = "Poll GitHub and run agent workflows")]
pub struct Cli {
    #[arg(long, default_value = "config.toml")]
    pub config: std::path::PathBuf,
}
