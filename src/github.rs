use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
}

pub async fn list_assigned_issues(
    client: &Client,
    owner: &str,
    repo: &str,
    assigned_to: &str,
    creator: &str,
    token: &str,
) -> Result<Vec<Issue>> {
    let mut all_issues: Vec<Issue> = Vec::new();
    let mut page: u32 = 1;

    loop {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/issues?state=open&assignee={assigned_to}&creator={creator}&per_page=100&page={page}"
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

        let page_issues: Vec<Issue> = resp
            .json()
            .await
            .context("deserializing GitHub issue list")?;

        if page_issues.is_empty() {
            break;
        }

        all_issues.extend(page_issues);
        page += 1;
    }

    Ok(all_issues)
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrReview {
    pub id: u64,
    pub pr_number: u64,
    pub user: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReviewResponse {
    id: u64,
    user: ReviewUser,
}

pub async fn list_pr_reviews(
    client: &Client,
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<Vec<PrReview>> {
    let mut all_reviews: Vec<PrReview> = Vec::new();
    let mut page: u32 = 1;

    loop {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls?state=open&per_page=100&page={page}"
        );
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "agent-orchestrator/0.1")
            .send()
            .await
            .context("sending GitHub pulls list request")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API returned {}: {}", status, body);
        }

        let page_prs: Vec<PullRequest> = resp
            .json()
            .await
            .context("deserializing GitHub pulls list")?;

        if page_prs.is_empty() {
            break;
        }

        for pr in &page_prs {
            let reviews_url = format!(
                "https://api.github.com/repos/{owner}/{repo}/pulls/{}/reviews",
                pr.number
            );
            let reviews_resp = client
                .get(&reviews_url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("User-Agent", "agent-orchestrator/0.1")
                .send()
                .await
                .context("sending GitHub PR reviews request")?;

            let reviews_status = reviews_resp.status();
            if !reviews_status.is_success() {
                let body = reviews_resp.text().await.unwrap_or_default();
                anyhow::bail!("GitHub API returned {}: {}", reviews_status, body);
            }

            let reviews: Vec<ReviewResponse> = reviews_resp
                .json()
                .await
                .context("deserializing GitHub PR reviews")?;

            for review in reviews {
                all_reviews.push(PrReview {
                    id: review.id,
                    pr_number: pr.number,
                    user: review.user.login,
                });
            }
        }

        page += 1;
    }

    Ok(all_reviews)
}

#[derive(Debug, Clone, Deserialize)]
struct PullRequest {
    number: u64,
}
