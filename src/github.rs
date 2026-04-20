use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
}

pub async fn list_assigned_issues(
    client: &Client,
    owner: &str,
    repo: &str,
    assigned_to: &str,
    token: &str,
) -> Result<Vec<Issue>> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/issues?state=open&assignee={assigned_to}&per_page=100"
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "agent-orchestrator/0.1")
        .send()
        .await
        .context("sending GitHub API request")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {}: {}", status, body);
    }

    resp.json::<Vec<Issue>>()
        .await
        .context("deserializing GitHub issue list")
}
