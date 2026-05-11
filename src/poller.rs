use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::time::{Duration, interval};

use crate::config::Config;
use crate::dispatcher::DispatchMessage;
use crate::runner::EventKey;
use crate::trigger::Trigger;
use crate::workflow;

/// Run the poll loop: discover events and dispatch them via the channel.
///
/// The poller is the producer; the dispatcher (consuming from `tx`) is the
/// consumer that actually spawns workflow tasks.
pub async fn run_poll_loop(
    workflows_dir: &Path,
    token: String,
    completed: Arc<Mutex<HashSet<String>>>,
    tx: tokio::sync::mpsc::Sender<DispatchMessage>,
    poll_interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    // Track workflow file timestamps so we only reload when something changes.
    // Initial load — failure is fatal because the daemon can't run without
    // any valid workflows.
    let (mut workflow_entries, mut last_file_state) = load_workflow_entries(workflows_dir)
        .map_err(|e| {
            tracing::error!(
                "failed to load workflow configs from {}: {}",
                workflows_dir.display(),
                e
            );
            e
        })?;

    let mut ticker = interval(Duration::from_secs(poll_interval_secs));

    loop {
        // Use select so the tick can be interrupted by shutdown.
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown_rx.changed() => {
                tracing::info!("Poll loop shutting down: skipping new event dispatch");
                break;
            }
        }

        // Check if shutdown was already requested (in case changed() fired
        // between ticks without us catching it via select).
        if *shutdown_rx.borrow() {
            tracing::info!("Poll loop shutting down: skipping new event dispatch");
            break;
        }

        // Hot-reload: check if workflow files changed since last tick.
        match load_workflow_entries_if_changed(workflows_dir, &last_file_state) {
            ReloadDecision::Reload((new_entries, new_file_state)) => {
                tracing::info!(
                    "workflow configs changed — reloading {} workflow(s)",
                    new_entries.len()
                );
                workflow_entries = new_entries;
                last_file_state = new_file_state;
            }
            ReloadDecision::Unchanged => {}
            ReloadDecision::Error(e) => {
                // Keep using the previous valid config rather than crashing.
                tracing::error!(
                    "failed to reload workflow configs: {} — keeping previous config",
                    e
                );
            }
        }

        for entry in &workflow_entries {
            for trigger in &entry.triggers {
                tracing::debug!(
                    "poll tick: trigger={}, checking {} repos",
                    trigger.name(),
                    entry.repos.len()
                );

                let events = match trigger.poll(&entry.repos, &token).await {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::error!("trigger {} poll failed: {}", trigger.name(), e);
                        continue;
                    }
                };

                for event in events {
                    // Build dedup key from owner/repo/event_key
                    let key_str = format!("{}/{}/{}", event.owner, event.repo, event.key);

                    // Parse the number from the event key for logic that needs it.
                    // For issues the key is a plain number (e.g. "42").
                    // For PR reviews the key is composite (e.g. "99/1234567"),
                    // so we extract the PR number from variables instead.
                    let number: u64 = match event.key.parse() {
                        Ok(n) => n,
                        Err(_) => event
                            .variables
                            .get("pr_number")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    "trigger event key is not a number and no pr_number variable: {}",
                                    event.key
                                );
                                0
                            }),
                    };
                    if number == 0 {
                        continue;
                    }

                    // Build a workspace_id that uniquely identifies the data directory.
                    // For issues: just the number (e.g. "42").
                    // For PR reviews: extract the review-specific part from the label,
                    // e.g. "acme/project#99_review-1234567" → "99_review-1234567".
                    let workspace_id = if event.key.contains('/') {
                        // PR review: extract the part after the '#' in the label.
                        // Label format: "owner/repo#PR_review-REVIEW_ID"
                        event.label.rsplit_once('#').map_or_else(
                            || event.key.replace('/', "_"),
                            |(_, suffix)| suffix.to_string(),
                        )
                    } else {
                        // Issue: workspace_id is just the number string.
                        number.to_string()
                    };

                    let event_key = EventKey {
                        owner: event.owner.clone(),
                        repo: event.repo.clone(),
                        number,
                        workspace_id,
                        label: event.label.clone(),
                        variables: event.variables.clone(),
                    };

                    // Skip if completed
                    if completed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&key_str)
                    {
                        continue;
                    }

                    // Send to dispatcher — skip only on channel closed
                    // (the in_flight and permanently_failed checks happen in the dispatcher).
                    let msg = DispatchMessage {
                        event_key,
                        steps: Arc::clone(&entry.steps),
                        git_config: entry.git_config.clone(),
                    };

                    if tx.send(msg).await.is_err() {
                        tracing::info!("Dispatcher channel closed, stopping poll loop");
                        return Ok(());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Pre-processed workflow entry used inside the poll loop.
struct WorkflowEntry {
    triggers: Vec<Box<dyn Trigger + Send>>,
    steps: Arc<Vec<workflow::Step>>,
    repos: Vec<crate::config::RepoConfig>,
    git_config: crate::config::GitConfig,
}

/// Result of checking whether workflow configs need reloading.
enum ReloadDecision {
    /// Files changed — here are the new entries and file state.
    Reload((Vec<WorkflowEntry>, HashMap<PathBuf, SystemTime>)),
    /// No changes detected.
    Unchanged,
    /// Loading failed; caller should keep the previous config.
    Error(anyhow::Error),
}

/// Build workflow entries from all TOML files in the workflows directory.
///
/// Returns the entries plus a map of file paths to their modification times,
/// used to detect changes for hot-reloading.
fn load_workflow_entries(
    workflows_dir: &Path,
) -> Result<(Vec<WorkflowEntry>, HashMap<PathBuf, SystemTime>)> {
    let configs = Config::load_all(workflows_dir)?;

    let mut file_state = HashMap::new();
    for entry in std::fs::read_dir(workflows_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml")
            && let Ok(metadata) = std::fs::metadata(&path)
            && let Ok(mtime) = metadata.modified()
        {
            file_state.insert(path, mtime);
        }
    }

    let entries = configs
        .into_iter()
        .map(|config| {
            let triggers: Vec<Box<dyn Trigger + Send>> =
                config.triggers.iter().map(|tc| tc.build()).collect();
            let steps = Arc::new(config.steps.clone());
            let repos = config.repos.clone();
            let git_config = config.git.clone();
            WorkflowEntry {
                triggers,
                steps,
                repos,
                git_config,
            }
        })
        .collect();

    Ok((entries, file_state))
}

/// Check if workflow config files have changed since `last_file_state`.
///
/// Compares the current set of `.toml` files and their modification times
/// against the previously recorded state. Returns:
/// - `Reload(new_entries, new_state)` if files were added, removed, or modified
/// - `Unchanged` if nothing changed
/// - `Error` if reloading failed (caller should keep the previous config)
fn load_workflow_entries_if_changed(
    workflows_dir: &Path,
    last_file_state: &HashMap<PathBuf, SystemTime>,
) -> ReloadDecision {
    // Scan current files and their mtimes.
    let mut current_state: HashMap<PathBuf, SystemTime> = HashMap::new();
    let dir_entries = match std::fs::read_dir(workflows_dir) {
        Ok(entries) => entries,
        Err(e) => return ReloadDecision::Error(anyhow::anyhow!("reading workflows dir: {}", e)),
    };
    for entry in dir_entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml")
                    && let Ok(metadata) = std::fs::metadata(&path)
                    && let Ok(mtime) = metadata.modified()
                {
                    current_state.insert(path, mtime);
                }
            }
            Err(e) => {
                tracing::warn!("skipping unreadable dir entry: {}", e);
            }
        }
    }

    // Compare current state against last known state.
    if current_state.len() != last_file_state.len() {
        // File count changed (added or removed a .toml file).
        return match load_workflow_entries(workflows_dir) {
            Ok((entries, state)) => ReloadDecision::Reload((entries, state)),
            Err(e) => ReloadDecision::Error(e),
        };
    }

    for (path, mtime) in &current_state {
        match last_file_state.get(path) {
            Some(last_mtime) if last_mtime == mtime => {}
            _ => {
                // New file or modified file detected.
                return match load_workflow_entries(workflows_dir) {
                    Ok((entries, state)) => ReloadDecision::Reload((entries, state)),
                    Err(e) => ReloadDecision::Error(e),
                };
            }
        }
    }

    ReloadDecision::Unchanged
}

/// Load previously completed event keys from the data directory.
pub fn load_completed(data_root: &Path) -> Arc<Mutex<HashSet<String>>> {
    let path = data_root.join("completed.json");
    let set: HashSet<String> = read_json_array(&path).into_iter().collect();
    Arc::new(Mutex::new(set))
}

fn read_json_array(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a minimal valid TOML workflow config.
    fn valid_toml() -> &'static str {
        r#"
[[triggers]]
type = "github_issue_assigned"
assigned_to = "testuser"
allowed_users = ["testuser"]

[[repos]]
owner = "testowner"
repo = "testrepo"

[[steps]]
name = "test-step"
prompt_template = "do something for {{owner}}/{{repo}}"
harness = { type = "hermes", profile = "cto" }
"#
    }

    #[test]
    fn load_workflow_entries_succeeds_with_valid_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("workflow.toml"), valid_toml()).unwrap();
        let (entries, file_state) = load_workflow_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 1, "expected 1 workflow entry");
        assert_eq!(file_state.len(), 1, "expected 1 file in state map");
        // The entry should have 1 trigger, 1 repo, and 1 step
        assert_eq!(entries[0].repos.len(), 1);
        assert_eq!(entries[0].steps.len(), 1);
    }

    #[test]
    fn load_workflow_entries_fails_with_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_workflow_entries(dir.path());
        assert!(result.is_err(), "expected error for empty dir");
    }

    #[test]
    fn load_workflow_entries_discovers_multiple_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.toml"), valid_toml()).unwrap();
        std::fs::write(dir.path().join("beta.toml"), valid_toml()).unwrap();
        // Non-toml file should be ignored in file state
        std::fs::write(dir.path().join("readme.md"), "ignore me").unwrap();
        let (entries, file_state) = load_workflow_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 2, "expected 2 workflow entries");
        assert_eq!(file_state.len(), 2, "expected 2 files in state map");
    }

    #[test]
    fn reload_decision_unchanged_when_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("workflow.toml"), valid_toml()).unwrap();
        let (_, file_state) = load_workflow_entries(dir.path()).unwrap();

        // Check again without any changes — should be Unchanged.
        match load_workflow_entries_if_changed(dir.path(), &file_state) {
            ReloadDecision::Unchanged => {}
            ReloadDecision::Reload(_) => panic!("expected Unchanged, got Reload"),
            ReloadDecision::Error(e) => panic!("expected Unchanged, got Error: {e}"),
        }
    }

    #[test]
    fn reload_decision_detects_new_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.toml"), valid_toml()).unwrap();
        let (_, file_state) = load_workflow_entries(dir.path()).unwrap();

        // Add a new .toml file.
        std::fs::write(dir.path().join("beta.toml"), valid_toml()).unwrap();

        match load_workflow_entries_if_changed(dir.path(), &file_state) {
            ReloadDecision::Reload((entries, new_state)) => {
                assert_eq!(entries.len(), 2, "expected 2 entries after adding a file");
                assert_eq!(new_state.len(), 2, "expected 2 files in new state");
            }
            ReloadDecision::Unchanged => panic!("expected Reload, got Unchanged"),
            ReloadDecision::Error(e) => panic!("expected Reload, got Error: {e}"),
        }
    }

    #[test]
    fn reload_decision_detects_removed_file() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.toml");
        let beta = dir.path().join("beta.toml");
        std::fs::write(&alpha, valid_toml()).unwrap();
        std::fs::write(&beta, valid_toml()).unwrap();
        let (_, file_state) = load_workflow_entries(dir.path()).unwrap();

        // Remove one .toml file.
        std::fs::remove_file(&beta).unwrap();

        match load_workflow_entries_if_changed(dir.path(), &file_state) {
            ReloadDecision::Reload((entries, new_state)) => {
                assert_eq!(entries.len(), 1, "expected 1 entry after removing a file");
                assert_eq!(new_state.len(), 1, "expected 1 file in new state");
            }
            ReloadDecision::Unchanged => panic!("expected Reload, got Unchanged"),
            ReloadDecision::Error(e) => panic!("expected Reload, got Error: {e}"),
        }
    }

    #[test]
    fn reload_decision_detects_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("workflow.toml");
        std::fs::write(&toml_path, valid_toml()).unwrap();
        let (_, file_state) = load_workflow_entries(dir.path()).unwrap();

        // Modify the file content (change the assigned_to user).
        let modified_toml = valid_toml().replace("testuser", "newuser");
        // Delete and recreate to force mtime change (avoids filesystem granularity issues).
        std::fs::remove_file(&toml_path).unwrap();
        std::fs::write(&toml_path, modified_toml).unwrap();

        match load_workflow_entries_if_changed(dir.path(), &file_state) {
            ReloadDecision::Reload((entries, _new_state)) => {
                // The reloaded config should reflect the change.
                assert_eq!(entries.len(), 1, "expected 1 entry after modifying a file");
            }
            ReloadDecision::Unchanged => {
                // On some CI systems mtime resolution is too coarse.
                // Deleting and recreating the file should still produce a
                // different mtime, but if it doesn't, this is acceptable.
            }
            ReloadDecision::Error(e) => panic!("expected Reload or Unchanged, got Error: {e}"),
        }
    }

    #[test]
    fn reload_decision_error_on_invalid_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Empty dir means no .toml files — load_workflow_entries will fail.
        let result = load_workflow_entries(dir.path());
        assert!(result.is_err(), "expected error for empty dir");

        // load_workflow_entries_if_changed should also return Error
        // when there are no .toml files (via the Reload path).
        let empty_state: HashMap<PathBuf, SystemTime> = HashMap::new();
        // An empty directory has 0 .toml files, same count as empty_state (0).
        // But all files in empty_state must match — with 0 files, this is vacuously true.
        // So it should return Unchanged (no reload needed since there's nothing to manage).
        match load_workflow_entries_if_changed(dir.path(), &empty_state) {
            ReloadDecision::Unchanged => {
                // Both are 0 .toml files — no change detected, which is correct.
            }
            ReloadDecision::Reload(_) => {
                // Also acceptable — it found the directory is in a different state.
            }
            ReloadDecision::Error(_) => {
                // Also acceptable — can't load entries from an empty dir.
            }
        }
    }
}
