use crate::git;
use crate::hooks;
use crate::template::render;
use crate::workflow::Step;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::info_span;

/// Identifies a trigger event uniquely (issue, PR review, etc.).
pub struct EventKey {
    pub owner: String,
    pub repo: String,
    /// Opaque numeric identifier (issue number, review ID, etc.).
    /// Used for data directory paths — not for display.
    pub number: u64,
    /// Human-readable label for logging (e.g. "acme/project#42" for issues,
    /// "acme/project#review_1234567" for PR reviews).
    pub label: String,
    /// Trigger-specific template variables carried from the TriggerEvent.
    pub variables: HashMap<String, String>,
}

impl std::fmt::Display for EventKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// Run all workflow steps for a single trigger event.
/// Returns Ok(()) if all steps succeeded, Err if any step failed.
/// data_root is the base data/ directory (e.g. PathBuf::from("data")).
/// token is the GitHub token used for authenticating git operations.
/// current_exe is the path to this binary, used as GIT_ASKPASS helper.
/// show_logs controls whether harness output is also printed to the terminal.
pub async fn run_event(
    key: &EventKey,
    data_root: &Path,
    steps: &[Step],
    token: &str,
    current_exe: &Path,
    show_logs: bool,
) -> Result<()> {
    let issue_dir = data_root
        .join(&key.owner)
        .join(&key.repo)
        .join(key.number.to_string());
    fs::create_dir_all(&issue_dir)?;

    // Ensure a git clone of the repo exists and is up-to-date.
    let workspace_dir =
        git::ensure_workspace(data_root, &key.owner, &key.repo, token, current_exe)?;

    for (idx, step) in steps.iter().enumerate() {
        let error_path = issue_dir.join(format!("step_{:02}_{}.error", idx, step.name));
        let log_path = issue_dir.join(format!("step_{:02}_{}.log", idx, step.name));

        // Build global template variables.
        let mut vars: HashMap<&str, String> = [
            ("owner", key.owner.clone()),
            ("repo", key.repo.clone()),
            ("output_path", issue_dir.to_string_lossy().into_owned()),
            ("workspace", workspace_dir.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .collect();

        // Merge trigger-specific variables (e.g. issue_number, pr_number).
        // Trigger variables use owned Strings; we borrow them as &str.
        for (k, v) in &key.variables {
            vars.insert(k.as_str(), v.clone());
        }

        // Create a span for the entire step iteration — pre-hooks, agent,
        // post-hooks, and all their log output inherit this context.
        let harness = step.harness.build();
        let span = info_span!(
            "step",
            issue = %key,
            step_name = %step.name,
            harness = %harness.name(),
        );
        let _enter = span.enter();

        // --- Pre-hooks -----------------------------------------------------------
        for hook in &step.pre_hooks {
            hooks::run_hook(hook, &vars, &error_path, token, current_exe).map_err(|e| {
                tracing::error!(step = step.name, "pre-hook FAILED: {}", e);
                e
            })?;
        }

        tracing::info!("started");

        // Run the harness.
        let rendered_prompt = render(&step.prompt_template, &vars);
        harness
            .run_step(
                step,
                &workspace_dir,
                &rendered_prompt,
                &error_path,
                &key.to_string(),
                &crate::harness::LogConfig {
                    log_path: log_path.clone(),
                    show_logs,
                },
            )
            .await?;

        // --- Post-hooks ----------------------------------------------------------
        for hook in &step.post_hooks {
            hooks::run_hook(hook, &vars, &error_path, token, current_exe).map_err(|e| {
                tracing::error!(step = step.name, "post-hook FAILED: {}", e);
                e
            })?;
        }

        tracing::info!("completed");
    }

    Ok(())
}
