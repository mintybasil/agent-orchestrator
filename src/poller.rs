use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::time::{Duration, interval};

use crate::config::Config;
use crate::runner::{EventKey, run_workflow};
use crate::trigger::Trigger;
use crate::workflow;

#[derive(Serialize, Deserialize)]
struct FailedEntry {
    key: String,
    timestamp: String,
    error: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_poll_loop(
    configs: Vec<Config>,
    token: String,
    data_root: &Path,
    completed: Arc<Mutex<HashSet<String>>>,
    current_exe: &Path,
    show_logs: bool,
    concurrency_limit: usize,
    poll_interval_secs: u64,
) -> Result<()> {
    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let permanently_failed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let file_lock: Arc<std::sync::Mutex<()>> = Arc::new(std::sync::Mutex::new(()));

    // When limit is 0, treat as unlimited. Use a large semaphore permit count
    // (usize::MAX) so acquisition never blocks.
    let semaphore = Arc::new(Semaphore::new(if concurrency_limit == 0 {
        usize::MAX
    } else {
        concurrency_limit
    }));

    // Pre-build triggers and step lists per config so we don't re-parse
    // trigger configs every tick.
    let workflow_entries: Vec<WorkflowEntry> = configs
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

    let mut ticker = interval(Duration::from_secs(poll_interval_secs));

    loop {
        ticker.tick().await;

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

                    // Parse the issue number from the event key
                    let issue_number: u64 = match event.key.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            tracing::warn!("trigger event key is not a number: {}", event.key);
                            continue;
                        }
                    };

                    let event_key = EventKey {
                        owner: event.owner.clone(),
                        repo: event.repo.clone(),
                        number: issue_number,
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
                    tracing::info!("[{}] dispatching workflow", event_key);

                    let completed_clone = Arc::clone(&completed);
                    let in_flight_clone = Arc::clone(&in_flight);
                    let permanently_failed_clone = Arc::clone(&permanently_failed);
                    let file_lock_clone = Arc::clone(&file_lock);
                    let data_root_clone = data_root.to_path_buf();
                    let key_str_clone = key_str.clone();
                    let token_clone = token.clone();
                    let current_exe_clone = current_exe.to_path_buf();
                    let show_logs_clone = show_logs;
                    let failed_path = data_root.join("failed.json");
                    let completed_path = data_root.join("completed.json");
                    let steps_clone = Arc::clone(&entry.steps);
                    let git_config_clone = entry.git_config.clone();
                    let sem_clone = Arc::clone(&semaphore);

                    tokio::spawn(async move {
                        // Acquire semaphore permit — blocks if at capacity.
                        let _permit = sem_clone.acquire().await.expect("semaphore not closed");

                        let result = run_workflow(
                            &event_key,
                            &data_root_clone,
                            &steps_clone,
                            &token_clone,
                            &current_exe_clone,
                            show_logs_clone,
                            &git_config_clone,
                        )
                        .await;
                        // Permit dropped here, freeing a slot.

                        in_flight_clone
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&key_str_clone);

                        match result {
                            Ok(()) => {
                                tracing::info!("[{}] workflow completed", event_key);
                                // Add to completed set and persist
                                completed_clone
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(key_str_clone.clone());
                                append_completed(&completed_path, &key_str_clone, &file_lock_clone);
                            }
                            Err(e) => {
                                tracing::error!("[{}] workflow FAILED: {}", event_key, e);
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
}

/// Pre-processed workflow entry used inside the poll loop.
struct WorkflowEntry {
    triggers: Vec<Box<dyn Trigger + Send>>,
    steps: Arc<Vec<workflow::Step>>,
    repos: Vec<crate::config::RepoConfig>,
    git_config: crate::config::GitConfig,
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
