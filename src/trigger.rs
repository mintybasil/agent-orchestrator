//! Generalized trigger system.
//!
//! A trigger is what initiates a workflow run. Currently only
//! `github_issue_assigned` is supported, but the trait allows
//! adding PR review, cron, label-based, etc. in the future.

use crate::config::RepoConfig;
use anyhow::Result;

/// A single event produced by a trigger.
#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub owner: String,
    pub repo: String,
    /// Opaque identifier for dedup (e.g. "42" for an issue number).
    pub key: String,
    /// Human-readable label for logging.
    #[allow(dead_code)]
    pub label: String,
}

/// Config-side trigger definition deserialized from TOML.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerConfig {
    /// Poll GitHub for issues assigned to a specific user.
    GithubIssueAssigned {
        assigned_to: String,
        allowed_issue_creators: Vec<String>,
    },
    // Future variants:
    // PrReview { reviewers: Vec<String> },
    // Cron { schedule: String },
    // IssueLabel { labels: Vec<String> },
}

/// Runtime trigger: produces events that initiate workflow runs.
///
/// The trait uses `'static` return lifetime because all data needed
/// by the future is cloned into the async block, making self-referential
/// borrows unnecessary.
pub trait Trigger {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Fetch events that should trigger a workflow run.
    fn poll(
        &self,
        repos: &[RepoConfig],
        token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    >;
}

/// Build a runtime Trigger from its config.
impl TriggerConfig {
    pub fn build(&self) -> Box<dyn Trigger + Send> {
        match self {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_issue_creators,
            } => Box::new(GithubIssueAssignedTrigger {
                client: reqwest::Client::new(),
                assigned_to: assigned_to.clone(),
                allowed_issue_creators: allowed_issue_creators.clone(),
            }),
        }
    }
}

// --- GithubIssueAssigned Trigger ---

pub struct GithubIssueAssignedTrigger {
    client: reqwest::Client,
    assigned_to: String,
    allowed_issue_creators: Vec<String>,
}

impl Trigger for GithubIssueAssignedTrigger {
    fn name(&self) -> &str {
        "github_issue_assigned"
    }

    fn poll(
        &self,
        repos: &[RepoConfig],
        token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    > {
        let assigned_to = self.assigned_to.clone();
        let allowed_creators = self.allowed_issue_creators.clone();
        let client = self.client.clone();
        let repos: Vec<RepoConfig> = repos.to_vec();
        let token = token.to_string();

        Box::pin(async move {
            let mut events = Vec::new();
            for repo_cfg in &repos {
                let mut seen_numbers = std::collections::HashSet::new();
                for creator in &allowed_creators {
                    match crate::github::list_assigned_issues(
                        &client,
                        &repo_cfg.owner,
                        &repo_cfg.repo,
                        &assigned_to,
                        creator,
                        &token,
                    )
                    .await
                    {
                        Err(e) => {
                            tracing::error!(
                                "GitHub API error for {}/{} (creator={}): {}",
                                repo_cfg.owner,
                                repo_cfg.repo,
                                creator,
                                e
                            );
                        }
                        Ok(page) => {
                            for issue in page {
                                if seen_numbers.insert(issue.number) {
                                    events.push(TriggerEvent {
                                        owner: repo_cfg.owner.clone(),
                                        repo: repo_cfg.repo.clone(),
                                        key: issue.number.to_string(),
                                        label: format!(
                                            "{}/{}#{}",
                                            repo_cfg.owner, repo_cfg.repo, issue.number
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Ok(events)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_issue_assigned_deserializes() {
        let toml = r#"
type = "github_issue_assigned"
assigned_to = "alice"
allowed_issue_creators = ["bob", "carol"]
"#;
        let config: TriggerConfig = toml::from_str(toml).unwrap();
        match config {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_issue_creators,
            } => {
                assert_eq!(assigned_to, "alice");
                assert_eq!(allowed_issue_creators, vec!["bob", "carol"]);
            }
        }
    }

    #[test]
    fn build_github_issue_assigned() {
        let config = TriggerConfig::GithubIssueAssigned {
            assigned_to: "test".to_string(),
            allowed_issue_creators: vec!["test".to_string()],
        };
        let trigger = config.build();
        assert_eq!(trigger.name(), "github_issue_assigned");
    }
}
