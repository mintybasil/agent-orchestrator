//! Generalized trigger system.
//!
//! A trigger is what initiates a workflow run. Currently supports
//! `github_issue_assigned` and `github_pr_review`, but the trait allows
//! adding cron, label-based, etc. in the future.

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
    /// Trigger-specific template variables (e.g. "issue_number" for issues,
    /// "pr_number" for PR reviews). These are merged with global variables
    /// (owner, repo, output_path, workspace) in the runner.
    pub variables: std::collections::HashMap<String, String>,
}

/// Config-side trigger definition deserialized from TOML.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerConfig {
    /// Poll GitHub for issues assigned to a specific user.
    GithubIssueAssigned {
        assigned_to: String,
        allowed_users: Vec<String>,
    },
    /// Poll GitHub for PR reviews/comments by allowed users.
    GithubPrReview { allowed_users: Vec<String> },
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
                allowed_users,
            } => Box::new(GithubIssueAssignedTrigger {
                client: reqwest::Client::new(),
                assigned_to: assigned_to.clone(),
                allowed_users: allowed_users.clone(),
            }),
            TriggerConfig::GithubPrReview { allowed_users } => Box::new(GithubPrReviewTrigger {
                client: reqwest::Client::new(),
                allowed_users: allowed_users.clone(),
            }),
        }
    }
}

// --- GithubIssueAssigned Trigger ---

pub struct GithubIssueAssignedTrigger {
    client: reqwest::Client,
    assigned_to: String,
    allowed_users: Vec<String>,
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
        let allowed_users = self.allowed_users.clone();
        let client = self.client.clone();
        let repos: Vec<RepoConfig> = repos.to_vec();
        let token = token.to_string();

        Box::pin(async move {
            let mut events = Vec::new();
            for repo_cfg in &repos {
                let mut seen_numbers = std::collections::HashSet::new();
                for user in &allowed_users {
                    match crate::github::list_assigned_issues(
                        &client,
                        &repo_cfg.owner,
                        &repo_cfg.repo,
                        &assigned_to,
                        user,
                        &token,
                    )
                    .await
                    {
                        Err(e) => {
                            tracing::error!(
                                "GitHub API error for {}/{} (creator={}): {}",
                                repo_cfg.owner,
                                repo_cfg.repo,
                                user,
                                e
                            );
                        }
                        Ok(page) => {
                            for issue in page {
                                if seen_numbers.insert(issue.number) {
                                    let mut vars = std::collections::HashMap::new();
                                    vars.insert(
                                        "issue_number".to_string(),
                                        issue.number.to_string(),
                                    );
                                    events.push(TriggerEvent {
                                        owner: repo_cfg.owner.clone(),
                                        repo: repo_cfg.repo.clone(),
                                        key: issue.number.to_string(),
                                        label: format!(
                                            "{}/{}#{}",
                                            repo_cfg.owner, repo_cfg.repo, issue.number
                                        ),
                                        variables: vars,
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

// --- GithubPrReview Trigger ---

pub struct GithubPrReviewTrigger {
    client: reqwest::Client,
    allowed_users: Vec<String>,
}

impl Trigger for GithubPrReviewTrigger {
    fn name(&self) -> &str {
        "github_pr_review"
    }

    fn poll(
        &self,
        repos: &[RepoConfig],
        token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    > {
        let allowed_users = self.allowed_users.clone();
        let client = self.client.clone();
        let repos: Vec<RepoConfig> = repos.to_vec();
        let token = token.to_string();

        Box::pin(async move {
            let mut events = Vec::new();
            for repo_cfg in &repos {
                let mut seen_prs = std::collections::HashSet::new();
                match crate::github::list_pr_reviews(
                    &client,
                    &repo_cfg.owner,
                    &repo_cfg.repo,
                    &token,
                )
                .await
                {
                    Err(e) => {
                        tracing::error!(
                            "GitHub API error for {}/{} (pr_reviews): {}",
                            repo_cfg.owner,
                            repo_cfg.repo,
                            e
                        );
                    }
                    Ok(reviews) => {
                        for review in reviews {
                            if !allowed_users.contains(&review.user) {
                                continue;
                            }
                            if seen_prs.insert(review.pr_number) {
                                let mut vars = std::collections::HashMap::new();
                                vars.insert("pr_number".to_string(), review.pr_number.to_string());
                                events.push(TriggerEvent {
                                    owner: repo_cfg.owner.clone(),
                                    repo: repo_cfg.repo.clone(),
                                    key: review.pr_number.to_string(),
                                    label: format!(
                                        "{}/{}#{}",
                                        repo_cfg.owner, repo_cfg.repo, review.pr_number
                                    ),
                                    variables: vars,
                                });
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
allowed_users = ["bob", "carol"]
"#;
        let config: TriggerConfig = toml::from_str(toml).unwrap();
        match config {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_users,
            } => {
                assert_eq!(assigned_to, "alice");
                assert_eq!(allowed_users, vec!["bob", "carol"]);
            }
            TriggerConfig::GithubPrReview { .. } => {
                panic!("expected GithubIssueAssigned, got GithubPrReview");
            }
        }
    }

    #[test]
    fn build_github_issue_assigned() {
        let config = TriggerConfig::GithubIssueAssigned {
            assigned_to: "test".to_string(),
            allowed_users: vec!["test".to_string()],
        };
        let trigger = config.build();
        assert_eq!(trigger.name(), "github_issue_assigned");
    }

    #[test]
    fn github_pr_review_deserializes() {
        let toml = r#"
type = "github_pr_review"
allowed_users = ["alice", "bob"]
"#;
        let config: TriggerConfig = toml::from_str(toml).unwrap();
        match config {
            TriggerConfig::GithubPrReview { allowed_users } => {
                assert_eq!(allowed_users, vec!["alice", "bob"]);
            }
            _ => panic!("expected GithubPrReview variant"),
        }
    }

    #[test]
    fn build_github_pr_review() {
        let config = TriggerConfig::GithubPrReview {
            allowed_users: vec!["test".to_string()],
        };
        let trigger = config.build();
        assert_eq!(trigger.name(), "github_pr_review");
    }

    #[test]
    fn trigger_event_issue_carries_issue_number_variable() {
        let vars =
            std::collections::HashMap::from([("issue_number".to_string(), "42".to_string())]);
        let event = TriggerEvent {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            key: "42".to_string(),
            label: "acme/project#42".to_string(),
            variables: vars,
        };
        assert_eq!(event.variables.get("issue_number"), Some(&"42".to_string()));
        assert_eq!(event.variables.get("pr_number"), None);
    }

    #[test]
    fn trigger_event_pr_carries_pr_number_variable() {
        let vars = std::collections::HashMap::from([("pr_number".to_string(), "99".to_string())]);
        let event = TriggerEvent {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            key: "99".to_string(),
            label: "acme/project#99".to_string(),
            variables: vars,
        };
        assert_eq!(event.variables.get("pr_number"), Some(&"99".to_string()));
        assert_eq!(event.variables.get("issue_number"), None);
    }
}
