use crate::git;
use crate::hooks;
use crate::template::render;
use crate::workflow::Step;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::info_span;

/// Identifies a GitHub issue uniquely.
pub struct IssueKey {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl std::fmt::Display for IssueKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Run all workflow steps for a single issue.
/// Returns Ok(()) if all steps succeeded, Err if any step failed.
/// data_root is the base data/ directory (e.g. PathBuf::from("data")).
/// token is the GitHub token used for authenticating git operations.
/// current_exe is the path to this binary, used as GIT_ASKPASS helper.
pub async fn run_issue(
    key: &IssueKey,
    data_root: &Path,
    steps: &[Step],
    token: &str,
    current_exe: &Path,
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

        // Build template variables.
        let var_pairs: Vec<(String, String)> = vec![
            ("owner".to_string(), key.owner.clone()),
            ("repo".to_string(), key.repo.clone()),
            ("issue_number".to_string(), key.number.to_string()),
            (
                "output_path".to_string(),
                issue_dir.to_string_lossy().into_owned(),
            ),
            (
                "workspace".to_string(),
                workspace_dir.to_string_lossy().into_owned(),
            ),
        ];
        let vars: HashMap<&str, String> = var_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

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
            hooks::run_hook(hook, &vars, &error_path).map_err(|e| {
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
            )
            .await?;

        // --- Post-hooks ----------------------------------------------------------
        for hook in &step.post_hooks {
            hooks::run_hook(hook, &vars, &error_path).map_err(|e| {
                tracing::error!(step = step.name, "post-hook FAILED: {}", e);
                e
            })?;
        }

        tracing::info!("completed");
    }

    Ok(())
}
