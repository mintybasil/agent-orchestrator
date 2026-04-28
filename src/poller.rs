use anyhow::Result;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, interval};

use crate::config::Config;
use crate::github::list_assigned_issues;
use crate::runner::{IssueKey, run_issue};
use crate::workflow;

#[derive(Serialize, Deserialize)]
struct FailedEntry {
    key: String,
    timestamp: String,
    error: String,
}

pub async fn run_poll_loop(
    config: Config,
    token: String,
    data_root: &Path,
    completed: Arc<Mutex<HashSet<String>>>,
    workflow_steps: Vec<workflow::Step>,
    current_exe: &Path,
) -> Result<()> {
    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let permanently_failed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let file_lock: Arc<std::sync::Mutex<()>> = Arc::new(std::sync::Mutex::new(()));
    let client = Client::new();
    let mut ticker = interval(Duration::from_secs(config.poll_interval_secs));
    let workflow_steps = std::sync::Arc::new(workflow_steps);

    loop {
        ticker.tick().await;
        tracing::info!("poll tick: checking {} repos", config.repos.len());

        for repo_cfg in &config.repos {
            let mut seen_numbers = std::collections::HashSet::new();
            let mut issues = Vec::new();
            for creator in &config.allowed_issue_creators {
                match list_assigned_issues(
                    &client,
                    &repo_cfg.owner,
                    &repo_cfg.repo,
                    &config.assigned_to,
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
                                issues.push(issue);
                            }
                        }
                    }
                }
            }
            for issue in issues {
                let key_str = format!("{}/{}/{}", repo_cfg.owner, repo_cfg.repo, issue.number);
                let issue_key = IssueKey {
                    owner: repo_cfg.owner.clone(),
                    repo: repo_cfg.repo.clone(),
                    number: issue.number,
                };

                // Skip if completed
                if completed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&key_str)
                {
                    continue;
                }
                // Skip if permanently failed this run
                if permanently_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&key_str)
                {
                    continue;
                }
                // Skip if in-flight
                if in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&key_str)
                {
                    continue;
                }

                // Mark in-flight and spawn
                in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key_str.clone());
                tracing::info!("[{}] dispatching workflow", issue_key);

                let completed_clone = Arc::clone(&completed);
                let in_flight_clone = Arc::clone(&in_flight);
                let permanently_failed_clone = Arc::clone(&permanently_failed);
                let file_lock_clone = Arc::clone(&file_lock);
                let data_root_clone = data_root.to_path_buf();
                let key_str_clone = key_str.clone();
                let token_clone = token.clone();
                let current_exe_clone = current_exe.to_path_buf();
                let failed_path = data_root.join("failed.json");
                let completed_path = data_root.join("completed.json");
                let steps_clone = Arc::clone(&workflow_steps);

                tokio::spawn(async move {
                    let result = run_issue(
                        &issue_key,
                        &data_root_clone,
                        &steps_clone,
                        &token_clone,
                        &current_exe_clone,
                    )
                    .await;
                    in_flight_clone
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key_str_clone);

                    match result {
                        Ok(()) => {
                            tracing::info!("[{}] workflow completed", issue_key);
                            // Add to completed set and persist
                            completed_clone
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(key_str_clone.clone());
                            append_completed(&completed_path, &key_str_clone, &file_lock_clone);
                        }
                        Err(e) => {
                            tracing::error!("[{}] workflow FAILED: {}", issue_key, e);
                            // Prevent re-dispatch within this daemon run (in-memory only)
                            permanently_failed_clone
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(key_str_clone.clone());
                            append_failed(
                                &failed_path,
                                &key_str_clone,
                                &e.to_string(),
                                &file_lock_clone,
                            );
                        }
                    }
                });
            }
        }
    }
}

/// Append a key to completed.json (read-modify-write the JSON array).
fn append_completed(path: &Path, key: &str, file_lock: &std::sync::Mutex<()>) {
    let _guard = file_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut set: Vec<String> = read_json_array(path);
    if !set.contains(&key.to_string()) {
        set.push(key.to_string());
    }
    if let Err(e) = write_json(path, &set) {
        tracing::error!("failed to persist completed.json: {}", e);
    }
}

/// Append a failure entry to failed.json.
fn append_failed(path: &Path, key: &str, error: &str, file_lock: &std::sync::Mutex<()>) {
    let _guard = file_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<FailedEntry> = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => vec![],
    };
    entries.push(FailedEntry {
        key: key.to_string(),
        timestamp: Utc::now().to_rfc3339(),
        error: error.to_string(),
    });
    if let Err(e) = write_json(path, &entries) {
        tracing::error!("failed to persist failed.json: {}", e);
    }
}

fn read_json_array(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => vec![],
    }
}

fn write_json<T: Serialize>(path: &Path, val: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(val)?;
    std::fs::write(path, s)?;
    Ok(())
}

pub fn load_completed(data_root: &Path) -> Arc<Mutex<HashSet<String>>> {
    let path = data_root.join("completed.json");
    let set: HashSet<String> = read_json_array(&path).into_iter().collect();
    Arc::new(Mutex::new(set))
}
