use anyhow::Context;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub assigned_to: String,
    pub workflow_file: std::path::PathBuf,
    pub repos: Vec<RepoConfig>,
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
        toml::from_str(&text)
            .with_context(|| format!("parsing config from {:?}", path))
    }
}

#[derive(clap::Parser, Debug)]
#[command(name = "agent-orchestrator", about = "Poll GitHub and run agent workflows")]
pub struct Cli {
    #[arg(long, default_value = "config.toml")]
    pub config: std::path::PathBuf,
}
