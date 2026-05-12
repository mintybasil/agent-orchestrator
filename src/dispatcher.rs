//! Dispatcher: consumes events from the poller via an mpsc channel and
//! spawns workflow tasks, managing in_flight, permanently_failed, and
//! completed sets plus the concurrency semaphore.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

use crate::config::GitConfig;
use crate::runner::run_workflow;
use crate::trigger::EventKey;
use crate::workflow::Step;

/// Message sent from the poller to the dispatcher for each discovered event.
pub struct DispatchMessage {
    pub event_key: EventKey,
    pub steps: Arc<Vec<Step>>,
    pub git_config: GitConfig,
}

#[derive(Serialize, Deserialize)]
struct FailedEntry {
    key: String,
    timestamp: String,
    error: String,
}

/// Manages the execution queue: consumes `DispatchMessage`s from a channel,
/// deduplicates via in_flight/permanently_failed/completed sets, and throttles
/// concurrency via a semaphore.
pub struct Dispatcher {
    pub in_flight: Arc<Mutex<HashSet<String>>>,
    pub permanently_failed: Arc<Mutex<HashSet<String>>>,
    pub completed: Arc<Mutex<HashSet<String>>>,
    file_lock: Arc<Mutex<()>>,
    semaphore: Arc<Semaphore>,
    data_root: PathBuf,
    token: String,
    current_exe: PathBuf,
    show_logs: bool,
}

impl Dispatcher {
    pub fn new(
        data_root: PathBuf,
        token: String,
        current_exe: PathBuf,
        show_logs: bool,
        concurrency_limit: usize,
        completed: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(if concurrency_limit == 0 {
            Semaphore::MAX_PERMITS
        } else {
            concurrency_limit
        }));

        Self {
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            permanently_failed: Arc::new(Mutex::new(HashSet::new())),
            completed,
            file_lock: Arc::new(Mutex::new(())),
            semaphore,
            data_root,
            token,
            current_exe,
            show_logs,
        }
    }

    /// Run the dispatch loop, consuming messages from `rx`.
    ///
    /// Returns when the sender side is dropped (i.e. the poller has shut down).
    pub async fn run(self, mut rx: tokio::sync::mpsc::Receiver<DispatchMessage>) {
        let mut active_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        while let Some(msg) = rx.recv().await {
            let key_str = format!(
                "{}/{}/{}",
                msg.event_key.owner, msg.event_key.repo, msg.event_key.workspace_id
            );

            // Skip if completed
            if self
                .completed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key_str)
            {
                continue;
            }
            // Skip if permanently failed this run
            if self
                .permanently_failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key_str)
            {
                continue;
            }
            // Skip if already in-flight
            if self
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key_str)
            {
                continue;
            }

            // Mark in-flight and spawn
            self.in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key_str.clone());

            tracing::info!("[{}] dispatching workflow", msg.event_key);

            let completed = Arc::clone(&self.completed);
            let in_flight = Arc::clone(&self.in_flight);
            let permanently_failed = Arc::clone(&self.permanently_failed);
            let file_lock = Arc::clone(&self.file_lock);
            let semaphore = Arc::clone(&self.semaphore);
            let data_root = self.data_root.clone();
            let token = self.token.clone();
            let current_exe = self.current_exe.clone();
            let show_logs = self.show_logs;

            let event_key = msg.event_key;
            let steps = msg.steps;
            let git_config = msg.git_config;
            let key_str_clone = key_str.clone();

            let handle = tokio::spawn(async move {
                // Acquire semaphore permit — blocks if at capacity.
                let _permit = semaphore.acquire().await.expect("semaphore not closed");

                let result = run_workflow(
                    &event_key,
                    &data_root,
                    &steps,
                    &token,
                    &current_exe,
                    show_logs,
                    &git_config,
                )
                .await;
                // Permit dropped here, freeing a slot.

                in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key_str_clone);

                let completed_path = data_root.join("completed.json");
                let failed_path = data_root.join("failed.json");

                match result {
                    Ok(()) => {
                        tracing::info!("[{}] workflow completed", event_key);
                        completed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key_str_clone.clone());
                        append_completed(&completed_path, &key_str_clone, &file_lock);
                    }
                    Err(e) => {
                        tracing::error!("[{}] workflow FAILED: {}", event_key, e);
                        permanently_failed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key_str_clone.clone());
                        append_failed(&failed_path, &key_str_clone, &e.to_string(), &file_lock);
                    }
                }
            });
            active_tasks.push(handle);

            // Periodically clean up finished handles to avoid unbounded growth.
            active_tasks.retain(|h| !h.is_finished());
        }

        // Channel closed (poller shut down) — wait for active workflows to drain.
        active_tasks.retain(|h| !h.is_finished());
        if !active_tasks.is_empty() {
            tracing::info!(
                "Waiting for {} active workflow(s) to complete…",
                active_tasks.len()
            );
            for handle in active_tasks {
                let _ = handle.await;
            }
        }
        tracing::info!("All active workflows completed. Dispatcher exiting.");
    }

    /// Returns true when nothing is in flight.
    #[allow(dead_code)]
    pub fn is_idle(&self) -> bool {
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// Append a key to completed.json (read-modify-write the JSON array).
fn append_completed(path: &Path, key: &str, file_lock: &Mutex<()>) {
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
fn append_failed(path: &Path, key: &str, error: &str, file_lock: &Mutex<()>) {
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

fn write_json<T: Serialize>(path: &Path, val: &T) -> anyhow::Result<()> {
    let s = serde_json::to_string_pretty(val)?;
    // Write to a temp file in the same directory, then atomically rename.
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, &s)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_idle_when_nothing_in_flight() {
        let dispatcher = Dispatcher::new(
            PathBuf::from("/tmp/test"),
            "token".to_string(),
            PathBuf::from("/usr/local/bin/gost"),
            false,
            0,
            Arc::new(Mutex::new(HashSet::new())),
        );
        assert!(dispatcher.is_idle());
    }

    #[test]
    fn test_not_idle_when_in_flight_workflows() {
        let completed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let dispatcher = Dispatcher::new(
            PathBuf::from("/tmp/test"),
            "token".to_string(),
            PathBuf::from("/usr/local/bin/gost"),
            false,
            0,
            completed,
        );
        dispatcher
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("owner/repo/42".to_string());
        assert!(!dispatcher.is_idle());
    }

    #[test]
    fn test_write_json_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        let data = vec!["test1".to_string(), "test2".to_string()];

        write_json(&path, &data).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"test1\""));
        assert!(content.contains("\"test2\""));

        // Ensure no .tmp file is left behind
        let tmp_path = path.with_extension("tmp");
        assert!(!tmp_path.exists());
    }

    #[test]
    fn test_append_completed_adds_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        let lock = Mutex::new(());

        append_completed(&path, "owner/repo/42", &lock);

        let entries = read_json_array(&path);
        assert!(entries.contains(&"owner/repo/42".to_string()));
    }

    #[test]
    fn test_append_completed_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        let lock = Mutex::new(());

        append_completed(&path, "owner/repo/42", &lock);
        append_completed(&path, "owner/repo/42", &lock);

        let entries = read_json_array(&path);
        assert_eq!(entries.iter().filter(|e| **e == "owner/repo/42").count(), 1);
    }

    #[test]
    fn test_append_failed_adds_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.json");
        let lock = Mutex::new(());

        append_failed(&path, "owner/repo/42", "something went wrong", &lock);

        let content = std::fs::read_to_string(&path).unwrap();
        let entries: Vec<FailedEntry> = serde_json::from_str(&content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "owner/repo/42");
        assert_eq!(entries[0].error, "something went wrong");
    }

    #[test]
    fn test_dispatcher_new_semaphore_limits() {
        // concurrency_limit=0 → usize::MAX permits
        let d0 = Dispatcher::new(
            PathBuf::from("/tmp"),
            "t".into(),
            PathBuf::from("/bin"),
            false,
            0,
            Arc::new(Mutex::new(HashSet::new())),
        );
        // We can't directly inspect the semaphore permit count, but we can
        // verify the Dispatcher was constructed without panic.

        // concurrency_limit=4 → 4 permits
        let d4 = Dispatcher::new(
            PathBuf::from("/tmp"),
            "t".into(),
            PathBuf::from("/bin"),
            false,
            4,
            Arc::new(Mutex::new(HashSet::new())),
        );
        // Same — just verifying construction succeeds.
        assert!(d4.in_flight.lock().unwrap().is_empty());
        assert!(d0.in_flight.lock().unwrap().is_empty());
    }
}
