//! Generalized trigger system.
//!
//! A trigger is what initiates a workflow run. The `Trigger` trait is agnostic
//! to the event source — it defines only how to discover and describe events.
//! Each concrete implementation (GitHub, local file, cron, etc.) owns its own
//! credentials and produces `TriggerEvent`s with source-specific variables.
//!
//! Adding a new trigger type:
//! 1. Add a variant to `TriggerConfig` in this file
//! 2. Add a struct implementing `Trigger`
//! 3. Add a match arm in `TriggerConfig::build()`

use crate::config::RepoConfig;
use crate::github::GitHubClient;
use anyhow::Result;

/// Identifies a trigger event uniquely (issue, PR review, local file, etc.).
///
/// Used by the runner to construct workspace directory paths, template
/// variables, and log labels. Moved here from runner.rs so that TriggerEvent
/// can produce it directly via `to_event_key()`.
#[derive(Debug)]
pub struct EventKey {
    pub owner: String,
    pub repo: String,
    /// Opaque numeric identifier (issue number, PR number, etc.).
    /// 0 when not applicable (e.g. cron or local file triggers).
    pub number: u64,
    /// Unique workspace identifier for data directory paths.
    /// For issues this is just the number (e.g. "42"),
    /// for PR reviews this includes the review ID (e.g. "99_review-1234567"),
    /// for local file triggers this is derived from the file name.
    pub workspace_id: String,
    /// Human-readable label for logging (e.g. "acme/project#42" for issues,
    /// "acme/project#99_review-1234567" for PR reviews).
    pub label: String,
    /// Trigger-specific template variables carried from the TriggerEvent.
    pub variables: std::collections::HashMap<String, String>,
}

impl std::fmt::Display for EventKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// A single event produced by a trigger.
///
/// Each trigger implementation populates all fields. The `workspace_id` and
/// `number` fields allow the event source to control how the data directory
/// is named and whether a numeric identifier is available.
#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub owner: String,
    pub repo: String,
    /// Opaque identifier for dedup (e.g. "42" for an issue number,
    /// "99/1234567" for a PR review where 99 is the PR number).
    pub key: String,
    /// Human-readable label for logging.
    pub label: String,
    /// Unique workspace identifier for data directory paths.
    /// For issues this is the issue number (e.g. "42"),
    /// for PR reviews this includes the review ID (e.g. "99_review-1234567"),
    /// for non-GitHub triggers this is a source-specific string.
    pub workspace_id: String,
    /// Opaque numeric identifier (issue number, PR number, etc.).
    /// 0 when not applicable (e.g. cron or local file triggers).
    pub number: u64,
    /// Trigger-specific template variables (e.g. "issue_number" for issues,
    /// "pr_number" and "review_id" for PR reviews). These are merged with global variables
    /// (owner, repo, output_path, workspace) in the runner.
    pub variables: std::collections::HashMap<String, String>,
}

impl TriggerEvent {
    /// Convert this TriggerEvent into an EventKey for use by the runner.
    ///
    /// This replaces the manual key-parsing logic that was previously in
    /// poller.rs. Each trigger knows best how to name its workspace and
    /// identify its events, so the conversion is a simple field mapping.
    pub fn to_event_key(&self) -> EventKey {
        EventKey {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            number: self.number,
            workspace_id: self.workspace_id.clone(),
            label: self.label.clone(),
            variables: self.variables.clone(),
        }
    }
}

/// Config-side trigger definition deserialized from TOML.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerConfig {
    /// Poll GitHub for issues assigned to a specific user.
    GithubIssueAssigned {
        assigned_to: String,
        allowed_users: Vec<String>,
    },
    /// Poll GitHub for PR reviews/comments by allowed users.
    GithubPrReview { allowed_users: Vec<String> },
    /// Watch a local directory for files matching a glob pattern.
    /// Each matching file produces a TriggerEvent with the filename as key.
    LocalFile {
        /// Directory to watch for files.
        path: String,
        /// Glob pattern to match files (default: "*" — all files).
        #[serde(default = "default_glob")]
        pattern: String,
    },
}

fn default_glob() -> String {
    "*".to_string()
}

/// Runtime trigger: produces events that initiate workflow runs.
///
/// The trait uses `'static` return lifetime because all data needed
/// by the future is cloned into the async block, making self-referential
/// borrows unnecessary.
///
/// The `poll` method is agnostic to the event source — no GitHub-specific
/// arguments like `token` are passed. Each implementation owns its own
/// credentials (injected at construction time via `TriggerConfig::build`).
pub trait Trigger {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Fetch events that should trigger a workflow run.
    fn poll(
        &self,
        repos: &[RepoConfig],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    >;
}

/// Build a runtime Trigger from its config.
///
/// The `token` parameter is used by GitHub triggers for API authentication.
/// Non-GitHub triggers ignore it.
impl TriggerConfig {
    pub fn build(&self, token: &str) -> Box<dyn Trigger + Send> {
        match self {
            TriggerConfig::GithubIssueAssigned {
                assigned_to,
                allowed_users,
            } => Box::new(GithubIssueAssignedTrigger {
                assigned_to: assigned_to.clone(),
                allowed_users: allowed_users.clone(),
                token: token.to_string(),
            }),
            TriggerConfig::GithubPrReview { allowed_users } => Box::new(GithubPrReviewTrigger {
                allowed_users: allowed_users.clone(),
                token: token.to_string(),
            }),
            TriggerConfig::LocalFile { path, pattern } => Box::new(LocalFileTrigger {
                directory: path.clone(),
                pattern: pattern.clone(),
            }),
        }
    }
}

// --- GithubIssueAssigned Trigger ---

pub struct GithubIssueAssignedTrigger {
    assigned_to: String,
    allowed_users: Vec<String>,
    token: String,
}

impl Trigger for GithubIssueAssignedTrigger {
    fn name(&self) -> &str {
        "github_issue_assigned"
    }

    fn poll(
        &self,
        repos: &[RepoConfig],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    > {
        let assigned_to = self.assigned_to.clone();
        let allowed_users = self.allowed_users.clone();
        let repos: Vec<RepoConfig> = repos.to_vec();
        let token = self.token.clone();
        let client = GitHubClient::new(token);

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
                                        workspace_id: issue.number.to_string(),
                                        number: issue.number,
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
    allowed_users: Vec<String>,
    token: String,
}

impl Trigger for GithubPrReviewTrigger {
    fn name(&self) -> &str {
        "github_pr_review"
    }

    fn poll(
        &self,
        repos: &[RepoConfig],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    > {
        let allowed_users = self.allowed_users.clone();
        let repos: Vec<RepoConfig> = repos.to_vec();
        let token = self.token.clone();
        let client = GitHubClient::new(token);

        Box::pin(async move {
            let mut events = Vec::new();
            for repo_cfg in &repos {
                let mut seen_reviews = std::collections::HashSet::new();
                match crate::github::list_pr_reviews(&client, &repo_cfg.owner, &repo_cfg.repo).await
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
                            if seen_reviews.insert(review.id) {
                                let mut vars = std::collections::HashMap::new();
                                vars.insert("pr_number".to_string(), review.pr_number.to_string());
                                vars.insert("review_id".to_string(), review.id.to_string());
                                let workspace_id =
                                    format!("{}_review-{}", review.pr_number, review.id);
                                events.push(TriggerEvent {
                                    owner: repo_cfg.owner.clone(),
                                    repo: repo_cfg.repo.clone(),
                                    key: format!("{}/{}", review.pr_number, review.id),
                                    label: format!(
                                        "{}/{}#{}_review-{}",
                                        repo_cfg.owner, repo_cfg.repo, review.pr_number, review.id
                                    ),
                                    workspace_id,
                                    number: review.pr_number,
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

// --- LocalFile Trigger ---

/// Watches a local directory for files matching a glob pattern.
/// Each matching file produces a TriggerEvent with the filename as key.
/// This trigger is primarily useful for testing and local workflows
/// that don't require a GitHub connection.
pub struct LocalFileTrigger {
    directory: String,
    pattern: String,
}

impl Trigger for LocalFileTrigger {
    fn name(&self) -> &str {
        "local_file"
    }

    fn poll(
        &self,
        _repos: &[RepoConfig],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<TriggerEvent>>> + Send + 'static>,
    > {
        let directory = self.directory.clone();
        let pattern = self.pattern.clone();

        Box::pin(async move {
            let mut events = Vec::new();
            let dir = std::path::Path::new(&directory);

            if !dir.is_dir() {
                tracing::warn!(
                    "local_file trigger: directory does not exist: {}",
                    directory
                );
                return Ok(events);
            }

            let glob_pattern = glob::Pattern::new(&pattern)
                .map_err(|e| anyhow::anyhow!("invalid glob pattern '{}': {}", pattern, e))?;

            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();

                if !path.is_file() {
                    continue;
                }

                let file_name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                if !glob_pattern.matches(&file_name) {
                    continue;
                }

                // Use the file stem (without extension) as a readable key,
                // and the full filename as the workspace_id.
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&file_name)
                    .to_string();

                events.push(TriggerEvent {
                    owner: "local".to_string(),
                    repo: "local".to_string(),
                    key: file_name.clone(),
                    label: format!("local:{}", file_name),
                    workspace_id: stem.clone(),
                    number: 0,
                    variables: std::collections::HashMap::from([
                        ("file_name".to_string(), file_name.clone()),
                        ("file_path".to_string(), path.to_string_lossy().into_owned()),
                    ]),
                });
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
            TriggerConfig::LocalFile { .. } => {
                panic!("expected GithubIssueAssigned, got LocalFile");
            }
        }
    }

    #[test]
    fn build_github_issue_assigned() {
        let config = TriggerConfig::GithubIssueAssigned {
            assigned_to: "test".to_string(),
            allowed_users: vec!["test".to_string()],
        };
        let trigger = config.build("fake-token");
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
        let trigger = config.build("fake-token");
        assert_eq!(trigger.name(), "github_pr_review");
    }

    #[test]
    fn local_file_deserializes() {
        let toml = r#"
type = "local_file"
path = "/tmp/watch"
"#;
        let config: TriggerConfig = toml::from_str(toml).unwrap();
        match config {
            TriggerConfig::LocalFile { path, pattern } => {
                assert_eq!(path, "/tmp/watch");
                assert_eq!(pattern, "*"); // default
            }
            _ => panic!("expected LocalFile variant"),
        }
    }

    #[test]
    fn local_file_deserializes_with_pattern() {
        let toml = r#"
type = "local_file"
path = "/tmp/watch"
pattern = "*.md"
"#;
        let config: TriggerConfig = toml::from_str(toml).unwrap();
        match config {
            TriggerConfig::LocalFile { path, pattern } => {
                assert_eq!(path, "/tmp/watch");
                assert_eq!(pattern, "*.md");
            }
            _ => panic!("expected LocalFile variant"),
        }
    }

    #[test]
    fn build_local_file() {
        let config = TriggerConfig::LocalFile {
            path: "/tmp/test".to_string(),
            pattern: "*.txt".to_string(),
        };
        let trigger = config.build(""); // No token needed for local trigger
        assert_eq!(trigger.name(), "local_file");
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
            workspace_id: "42".to_string(),
            number: 42,
            variables: vars,
        };
        assert_eq!(event.variables.get("issue_number"), Some(&"42".to_string()));
        assert_eq!(event.variables.get("pr_number"), None);
        assert_eq!(event.workspace_id, "42");
        assert_eq!(event.number, 42);
    }

    #[test]
    fn trigger_event_pr_carries_pr_number_variable() {
        let vars = std::collections::HashMap::from([
            ("pr_number".to_string(), "99".to_string()),
            ("review_id".to_string(), "1234567".to_string()),
        ]);
        let event = TriggerEvent {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            key: "99/1234567".to_string(),
            label: "acme/project#99_review-1234567".to_string(),
            workspace_id: "99_review-1234567".to_string(),
            number: 99,
            variables: vars,
        };
        assert_eq!(event.variables.get("pr_number"), Some(&"99".to_string()));
        assert_eq!(
            event.variables.get("review_id"),
            Some(&"1234567".to_string())
        );
        assert_eq!(event.variables.get("issue_number"), None);
        assert_eq!(event.key, "99/1234567");
        assert_eq!(event.label, "acme/project#99_review-1234567");
        assert_eq!(event.workspace_id, "99_review-1234567");
        assert_eq!(event.number, 99);
    }

    #[test]
    fn trigger_event_cron_style_has_no_numeric_id() {
        // Simulate a cron-style event: no numeric ID, custom workspace_id.
        let event = TriggerEvent {
            owner: "local".to_string(),
            repo: "local".to_string(),
            key: "nightly-build".to_string(),
            label: "cron:nightly-build".to_string(),
            workspace_id: "nightly-build".to_string(),
            number: 0,
            variables: std::collections::HashMap::from([(
                "schedule".to_string(),
                "0 0 * * *".to_string(),
            )]),
        };
        assert_eq!(event.number, 0);
        assert_eq!(event.workspace_id, "nightly-build");
    }

    #[test]
    fn trigger_event_file_style_has_filename() {
        // Simulate a local-file event.
        let event = TriggerEvent {
            owner: "local".to_string(),
            repo: "local".to_string(),
            key: "todo.md".to_string(),
            label: "local:todo.md".to_string(),
            workspace_id: "todo".to_string(),
            number: 0,
            variables: std::collections::HashMap::from([
                ("file_name".to_string(), "todo.md".to_string()),
                ("file_path".to_string(), "/tmp/watch/todo.md".to_string()),
            ]),
        };
        assert_eq!(event.number, 0);
        assert_eq!(
            event.variables.get("file_name"),
            Some(&"todo.md".to_string())
        );
    }

    #[test]
    fn trigger_event_to_event_key_maps_fields() {
        let vars =
            std::collections::HashMap::from([("issue_number".to_string(), "42".to_string())]);
        let event = TriggerEvent {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            key: "42".to_string(),
            label: "acme/project#42".to_string(),
            workspace_id: "42".to_string(),
            number: 42,
            variables: vars,
        };
        let ek = event.to_event_key();
        assert_eq!(ek.owner, "acme");
        assert_eq!(ek.repo, "project");
        assert_eq!(ek.number, 42);
        assert_eq!(ek.workspace_id, "42");
        assert_eq!(ek.label, "acme/project#42");
        assert_eq!(ek.variables.get("issue_number"), Some(&"42".to_string()));
    }

    #[test]
    fn trigger_event_to_event_key_for_pr_review() {
        let vars = std::collections::HashMap::from([
            ("pr_number".to_string(), "99".to_string()),
            ("review_id".to_string(), "1234567".to_string()),
        ]);
        let event = TriggerEvent {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            key: "99/1234567".to_string(),
            label: "acme/project#99_review-1234567".to_string(),
            workspace_id: "99_review-1234567".to_string(),
            number: 99,
            variables: vars,
        };
        let ek = event.to_event_key();
        assert_eq!(ek.number, 99);
        assert_eq!(ek.workspace_id, "99_review-1234567");
        assert_eq!(ek.variables.get("review_id"), Some(&"1234567".to_string()));
    }

    #[test]
    fn trigger_config_rejects_unknown_fields() {
        let toml = r#"
type = "github_issue_assigned"
assigned_to = "alice"
allowed_users = ["bob"]
typo_field = "oops"
"#;
        let result = toml::from_str::<TriggerConfig>(toml);
        assert!(
            result.is_err(),
            "expected unknown field to be rejected, got: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field"),
            "error should mention 'unknown field', got: {err}"
        );
    }

    #[test]
    fn local_file_trigger_poll_discovers_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("task1.md"), "do something").unwrap();
        std::fs::write(dir.path().join("task2.txt"), "do other").unwrap();
        std::fs::write(dir.path().join("ignore.py"), "skip").unwrap();

        let trigger = LocalFileTrigger {
            directory: dir.path().to_string_lossy().into_owned(),
            pattern: "*.md".to_string(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events = rt.block_on(trigger.poll(&[])).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, "task1.md");
        assert_eq!(events[0].workspace_id, "task1");
        assert_eq!(events[0].number, 0);
        assert_eq!(events[0].owner, "local");
        assert_eq!(
            events[0].variables.get("file_name"),
            Some(&"task1.md".to_string())
        );
    }

    #[test]
    fn local_file_trigger_poll_handles_missing_dir() {
        let trigger = LocalFileTrigger {
            directory: "/nonexistent/path".to_string(),
            pattern: "*".to_string(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events = rt.block_on(trigger.poll(&[])).unwrap();
        assert!(events.is_empty());
    }
}
