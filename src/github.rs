//! GitHub API client with rate limit tracking and adaptive backoff.
//!
//! The [`GitHubClient`] wraps a `reqwest::Client` and manages authentication
//! headers, rate limit monitoring (via `X-RateLimit-Remaining` /
//! `X-RateLimit-Reset` response headers), and automatic sleep-retry on 403
//! responses caused by rate limiting.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::time::sleep;

/// Number of remaining requests below which we proactively sleep until reset.
const RATE_LIMIT_LOW_THRESHOLD: u32 = 5;

/// Minimum sleep duration when we cannot parse the reset timestamp.
const MIN_BACKOFF_DURATION: Duration = Duration::from_secs(1);

/// Maximum sleep duration to prevent waiting indefinitely on bad reset timestamps.
const MAX_BACKOFF_DURATION: Duration = Duration::from_secs(300);

/// GitHub API client with built-in rate limiting.
///
/// Wraps a `reqwest::Client` and centralises:
/// - Authentication headers (Bearer token)
/// - Common API headers (Accept, X-GitHub-Api-Version, User-Agent)
/// - Rate limit header extraction and logging
/// - Adaptive backoff when remaining budget is low or a 403 is returned
#[derive(Debug)]
pub struct GitHubClient {
    client: Client,
    token: String,
    /// Last observed remaining request count (atomic for lock-free reads).
    remaining: AtomicU32,
}

impl Clone for GitHubClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            token: self.token.clone(),
            remaining: AtomicU32::new(self.remaining.load(Ordering::Relaxed)),
        }
    }
}

impl GitHubClient {
    /// Create a new client from a GitHub personal access token.
    pub fn new(token: String) -> Self {
        Self {
            client: Client::new(),
            token,
            remaining: AtomicU32::new(u32::MAX),
        }
    }

    /// Send an HTTP request with common GitHub API headers and rate limit
    /// handling.
    ///
    /// Adds `Authorization`, `Accept`, `X-GitHub-Api-Version`, and `User-Agent`
    /// headers, then:
    ///
    /// 1. Sends the request.
    /// 2. Extracts `X-RateLimit-Remaining` and `X-RateLimit-Reset` from the
    ///    response headers.
    /// 3. If the response is 403 Forbidden and rate-limit headers indicate we
    ///    were rate limited, sleeps until the reset time and retries once.
    /// 4. If remaining is critically low (< {RATE_LIMIT_LOW_THRESHOLD}) on a
    ///    successful response, proactively sleeps until the reset time to
    ///    avoid hitting the limit.
    pub async fn send_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let request = request
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "agent-orchestrator/0.1");

        let resp = request.send().await.context("sending GitHub API request")?;

        // Extract and track rate limit headers.
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u32>().ok());

        let reset = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        if let Some(rem) = remaining {
            self.remaining.store(rem, Ordering::Relaxed);
            tracing::debug!("GitHub API rate limit: {} remaining", rem);
        }

        // --- Handle 403 Forbidden (possible rate limit) ---
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            // Save the URL before consuming the response body.
            let url = resp.url().clone();

            // Check if this is a rate limit error by looking at the response
            // body and/or rate limit headers. GitHub returns 403 with a
            // "rate limit exceeded" message when you exceed the limit.
            let body = resp.text().await.unwrap_or_default();
            let is_rate_limited = body.contains("rate limit")
                || body.contains("abuse")
                || remaining.is_some_and(|r| r == 0);

            if is_rate_limited && let Some(sleep_dur) = calculate_sleep_until_reset(reset) {
                tracing::warn!(
                    "GitHub API rate limit hit. Sleeping {:.1}s until reset (remaining={}).",
                    sleep_dur.as_secs_f64(),
                    remaining.unwrap_or(0),
                );
                sleep(sleep_dur).await;

                // Retry once after sleeping. Build a fresh request since
                // the previous one was already consumed.
                let retry = self
                    .client
                    .get(url)
                    .header("Authorization", format!("Bearer {}", self.token))
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("User-Agent", "agent-orchestrator/0.1")
                    .send()
                    .await
                    .context("retrying GitHub API request after rate limit backoff")?;

                // Extract rate limit headers from retry response.
                let retry_remaining = retry
                    .headers()
                    .get("x-ratelimit-remaining")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u32>().ok());
                if let Some(rem) = retry_remaining {
                    self.remaining.store(rem, Ordering::Relaxed);
                    tracing::debug!("GitHub API rate limit after retry: {} remaining", rem);
                }

                return Ok(retry);
            }

            // Not a rate limit 403 — return the error.
            anyhow::bail!(
                "{}",
                format_github_error(reqwest::StatusCode::FORBIDDEN, &body)
            );
        }

        // --- Proactive backoff: remaining is critically low on a success ---
        if resp.status().is_success()
            && let Some(rem) = remaining
            && rem < RATE_LIMIT_LOW_THRESHOLD
            && let Some(sleep_dur) = calculate_sleep_until_reset(reset)
        {
            tracing::warn!(
                "GitHub API rate limit critically low ({remaining} remaining). \
                 Proactively sleeping {:.1}s until reset.",
                sleep_dur.as_secs_f64(),
                remaining = rem,
            );
            sleep(sleep_dur).await;
        }

        Ok(resp)
    }
}

/// Calculate the sleep duration until the `X-RateLimit-Reset` timestamp.
///
/// Returns `None` if the reset timestamp is missing or already in the past.
/// Clamps the sleep to [{MIN_BACKOFF_DURATION}, {MAX_BACKOFF_DURATION}].
fn calculate_sleep_until_reset(reset_epoch: Option<u64>) -> Option<Duration> {
    let reset_epoch = reset_epoch?;
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if reset_epoch <= now_epoch {
        // Reset time already passed.
        return Some(MIN_BACKOFF_DURATION);
    }

    let sleep_secs = reset_epoch - now_epoch;
    let sleep_dur =
        Duration::from_secs(sleep_secs).clamp(MIN_BACKOFF_DURATION, MAX_BACKOFF_DURATION);
    Some(sleep_dur)
}

#[derive(Debug, Deserialize)]
struct GithubErrorResponse {
    message: Option<String>,
}

fn format_github_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(error_resp) = serde_json::from_str::<GithubErrorResponse>(body)
        && let Some(msg) = error_resp.message
    {
        return format!("GitHub API returned {}: {}", status, msg);
    }
    format!(
        "GitHub API returned {} (body too large or not JSON)",
        status
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
}

pub async fn list_assigned_issues(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    assigned_to: &str,
    creator: &str,
) -> Result<Vec<Issue>> {
    let mut all_issues: Vec<Issue> = Vec::new();
    let mut page: u32 = 1;

    loop {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/issues?state=open&assignee={assigned_to}&creator={creator}&per_page=100&page={page}"
        );
        let resp = client.send_request(client.client.get(&url)).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("{}", format_github_error(status, &body));
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
    client: &GitHubClient,
    owner: &str,
    repo: &str,
) -> Result<Vec<PrReview>> {
    let mut all_reviews: Vec<PrReview> = Vec::new();
    let mut page: u32 = 1;

    loop {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/pulls?state=open&per_page=100&page={page}"
        );
        let resp = client.send_request(client.client.get(&url)).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("{}", format_github_error(status, &body));
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
                .send_request(client.client.get(&reviews_url))
                .await
                .context("sending GitHub PR reviews request")?;

            let reviews_status = reviews_resp.status();
            if !reviews_status.is_success() {
                let body = reviews_resp.text().await.unwrap_or_default();
                anyhow::bail!("{}", format_github_error(reviews_status, &body));
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


/// An issue where a specific user was @mentioned.
#[derive(Debug, Clone, Deserialize)]
pub struct MentionedIssue {
    pub number: u64,
}

/// List open issues in a repository where `mentioned_user` is @mentioned.
///
/// Uses the GitHub Search API with the query:
///   `mentions:USERNAME is:issue is:open repo:OWNER/REPO`
///
/// This finds issues where the user appears in the body, comments, or
/// was explicitly @mentioned, regardless of assignment status.
pub async fn list_mentioned_issues(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    mentioned_user: &str,
) -> Result<Vec<MentionedIssue>> {
    let mut all_issues: Vec<MentionedIssue> = Vec::new();
    let mut page: u32 = 1;

    loop {
        // GitHub Search API: q parameter encodes the query, per_page/page for pagination
        let url = format!(
            "https://api.github.com/search/issues?q=mentions%3A{mentioned_user}+is%3Aissue+is%3Aopen+repo%3A{owner}%2F{repo}&per_page=100&page={page}"
        );
        let resp = client.send_request(client.client.get(&url)).await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("{}", format_github_error(status, &body));
        }

        #[derive(Debug, Deserialize)]
        struct SearchResponse {
            items: Vec<MentionedIssue>,
        }

        let search: SearchResponse = resp
            .json()
            .await
            .context("deserializing GitHub search issues response")?;

        if search.items.is_empty() {
            break;
        }

        all_issues.extend(search.items);
        page += 1;
    }

    Ok(all_issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_github_error_json() {
        let status = reqwest::StatusCode::SERVICE_UNAVAILABLE;
        let body = r#"{"message": "No server available"}"#;
        assert_eq!(
            format_github_error(status, body),
            "GitHub API returned 503 Service Unavailable: No server available"
        );
    }

    #[test]
    fn test_format_github_error_html() {
        let status = reqwest::StatusCode::GATEWAY_TIMEOUT;
        let body = "<html><body>504 Gateway Timeout</body></html>";
        assert_eq!(
            format_github_error(status, body),
            "GitHub API returned 504 Gateway Timeout (body too large or not JSON)"
        );
    }

    #[test]
    fn test_format_github_error_json_without_message() {
        let status = reqwest::StatusCode::FORBIDDEN;
        let body = r#"{"documentation_url": "https://docs.github.com"}"#;
        assert_eq!(
            format_github_error(status, body),
            "GitHub API returned 403 Forbidden (body too large or not JSON)"
        );
    }

    #[test]
    fn test_github_client_new_stores_token() {
        let client = GitHubClient::new("ghp_test123".to_string());
        assert_eq!(client.token, "ghp_test123");
        assert_eq!(client.remaining.load(Ordering::Relaxed), u32::MAX);
    }

    #[test]
    fn test_github_client_clone_preserves_state() {
        let client = GitHubClient::new("ghp_test123".to_string());
        client.remaining.store(42, Ordering::Relaxed);
        let cloned = client.clone();
        assert_eq!(cloned.remaining.load(Ordering::Relaxed), 42);
        assert_eq!(cloned.token, "ghp_test123");
    }

    #[test]
    fn test_calculate_sleep_until_reset_future_timestamp() {
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Reset 10 seconds in the future.
        let reset_epoch = now_epoch + 10;
        let sleep_dur = calculate_sleep_until_reset(Some(reset_epoch)).unwrap();
        assert!(
            sleep_dur >= MIN_BACKOFF_DURATION,
            "sleep duration should be at least MIN_BACKOFF"
        );
        assert!(
            sleep_dur.as_secs() <= 10,
            "sleep duration should not exceed the reset window"
        );
    }

    #[test]
    fn test_calculate_sleep_until_reset_past_timestamp() {
        // Reset 100 seconds in the past — already expired.
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reset_epoch = now_epoch - 100;
        let sleep_dur = calculate_sleep_until_reset(Some(reset_epoch)).unwrap();
        assert_eq!(sleep_dur, MIN_BACKOFF_DURATION);
    }

    #[test]
    fn test_calculate_sleep_until_reset_none() {
        assert!(calculate_sleep_until_reset(None).is_none());
    }

    #[test]
    fn test_calculate_sleep_until_reset_clamps_max() {
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Reset 1 hour in the future — should be clamped to MAX.
        let reset_epoch = now_epoch + 3600;
        let sleep_dur = calculate_sleep_until_reset(Some(reset_epoch)).unwrap();
        assert_eq!(sleep_dur, MAX_BACKOFF_DURATION);
    }
}
