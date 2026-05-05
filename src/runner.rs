use crate::config::GitConfig;
use crate::git;
use crate::hooks;
use crate::template::render;
use crate::workflow::Step;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Identifies a trigger event uniquely (issue, PR review, etc.).
#[derive(Debug)]
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

struct StepContext {
    error_path: PathBuf,
    show_logs: bool,
    log_path: PathBuf,
    /// The directory from which the harness should be invoked.
    /// This may be the worktree, the repo, or the event workspace.
    work_dir: PathBuf,
    key: String,
    token: String,
    current_exe: PathBuf,
}

/// Shared context for all steps in a single event run.
struct RunContext {
    /// Per-event workspace directory (for logs, errors, output files).
    workspace_dir: PathBuf,
    /// Directory from which the harness and git hooks run.
    harness_work_dir: PathBuf,
    /// Template variables available to all steps.
    vars: HashMap<String, String>,
    /// Human-readable event label for logging.
    key: String,
    /// GitHub token for git auth.
    token: String,
    /// Path to this binary (used as GIT_ASKPASS helper).
    current_exe: PathBuf,
    /// Whether to also print harness output to terminal.
    show_logs: bool,
}

/// Run all workflow steps for a single trigger event.
/// Returns Ok(()) if all steps succeeded, Err if any step failed.
/// data_root is the base data/ directory (e.g. PathBuf::from("data")).
/// token is the GitHub token used for authenticating git operations.
/// current_exe is the path to this binary, used as GIT_ASKPASS helper.
/// show_logs controls whether harness output is also printed to the terminal.
/// git_config controls repo clone and worktree behavior.
#[instrument(skip(data_root, steps, token, current_exe, git_config))]
pub async fn run_event(
    key: &EventKey,
    data_root: &Path,
    steps: &[Step],
    token: &str,
    current_exe: &Path,
    show_logs: bool,
    git_config: &GitConfig,
) -> Result<()> {
    let workspace_dir = data_root
        .join(&key.owner)
        .join(&key.repo)
        .join(key.number.to_string());
    fs::create_dir_all(&workspace_dir)?;

    // Determine the repo path and optionally create a worktree.
    // If git.clone is false, no repo operations are performed.
    let repo_dir = if git_config.clone {
        Some(git::ensure_repo(
            data_root,
            &key.owner,
            &key.repo,
            &git_config.default_branch,
            token,
            current_exe,
        )?)
    } else {
        None
    };

    // If worktree is enabled, create a fresh worktree inside the workspace dir.
    let worktree_info = if git_config.worktree {
        let repo = repo_dir.as_ref().ok_or_else(|| {
            anyhow::anyhow!("git.worktree requires a repo (git.clone must be true)")
        })?;
        // Generate a unique branch name to avoid collisions with the main
        // checkout and across parallel/failed runs.  The `key` (e.g. "acme/project#42")
        // is already unique per-event, and the timestamp adds extra uniqueness
        // for re-runs of the same event after a prior failure.
        let branch_name = format!(
            "ao/{}-{}",
            key.label.replace(['/', '#'], "-"),
            Utc::now().timestamp()
        );
        let wt_name = format!("worktree-{}", key.number);
        let wt_path = workspace_dir.join(&wt_name);
        git::create_worktree(
            repo,
            &wt_path,
            &git_config.default_branch,
            &branch_name,
            token,
            current_exe,
        )?;
        Some((wt_path, branch_name))
    } else {
        None
    };

    // Determine the working directory for the harness:
    // - worktree path if worktree is enabled
    // - repo path if clone is enabled but no worktree
    // - event workspace directory otherwise
    let harness_work_dir = worktree_info
        .as_ref()
        .map(|(wt_path, _)| wt_path.clone())
        .or_else(|| repo_dir.clone())
        .unwrap_or_else(|| workspace_dir.clone());

    // Build global template variables.
    let mut vars: HashMap<String, String> = [
        ("owner".into(), key.owner.clone()),
        ("repo".into(), key.repo.clone()),
        (
            "output_path".into(),
            workspace_dir.clone().to_string_lossy().into_owned(),
        ),
        (
            // {{repo_path}} gives the path to the base repository clone.
            // Empty string when git.clone = false (no repo checkout).
            "repo_path".into(),
            repo_dir
                .clone()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    ]
    .into_iter()
    .collect();

    // Merge trigger-specific variables (e.g. issue_number, pr_number).
    for (k, v) in &key.variables {
        vars.insert(k.clone(), v.clone());
    }

    let ctx = RunContext {
        workspace_dir,
        harness_work_dir,
        vars,
        key: key.to_string(),
        token: token.into(),
        current_exe: current_exe.into(),
        show_logs,
    };

    let result = run_steps(steps, &ctx).await;

    // Clean up worktree if one was created.
    if let Some((ref wt_path, ref branch_name)) = worktree_info {
        let repo = repo_dir.as_ref().unwrap();
        if let Err(cleanup_err) = cleanup_worktree(
            repo,
            wt_path,
            branch_name,
            token,
            current_exe,
            &git_config.default_branch,
        ) {
            tracing::error!("worktree cleanup failed: {}", cleanup_err);
            // If the workflow itself succeeded, the cleanup error becomes the result.
            // If it already failed, keep the original error.
            if result.is_ok() {
                return Err(cleanup_err);
            }
        }
    }

    result
}

/// Run all workflow steps sequentially.
async fn run_steps(steps: &[Step], ctx: &RunContext) -> Result<()> {
    for (idx, step) in steps.iter().enumerate() {
        let error_path = ctx
            .workspace_dir
            .join(format!("step_{:02}_{}.error", idx, step.name));
        let log_path = ctx
            .workspace_dir
            .join(format!("step_{:02}_{}.log", idx, step.name));

        let step_ctx = StepContext {
            error_path,
            show_logs: ctx.show_logs,
            log_path,
            work_dir: ctx.harness_work_dir.clone(),
            key: ctx.key.clone(),
            token: ctx.token.clone(),
            current_exe: ctx.current_exe.clone(),
        };

        run_step(step, &step_ctx, &ctx.vars).await?
    }

    Ok(())
}

#[instrument(skip(step, ctx, vars), fields(step=step.name, key = ctx.key))]
async fn run_step(step: &Step, ctx: &StepContext, vars: &HashMap<String, String>) -> Result<()> {
    tracing::info!("Starting step");
    // --- Pre-hooks -----------------------------------------------------------
    for hook in &step.pre_hooks {
        hooks::run_hook(hook, vars, &ctx.error_path, &ctx.token, &ctx.current_exe).map_err(
            |e| {
                tracing::error!("pre-hook FAILED: {}", e);
                e
            },
        )?;
    }

    let harness = step.harness.build();
    let rendered_prompt = render(&step.prompt_template, vars);

    tracing::info!("Launching {} harness", harness.name());
    harness
        .run_step(
            step,
            &ctx.work_dir,
            &rendered_prompt,
            &ctx.error_path,
            &ctx.key,
            &crate::harness::LogConfig {
                log_path: ctx.log_path.clone(),
                show_logs: ctx.show_logs,
            },
        )
        .await?;

    tracing::info!("Harness invocation complete");

    // --- Post-hooks ----------------------------------------------------------
    for hook in &step.post_hooks {
        hooks::run_hook(hook, vars, &ctx.error_path, &ctx.token, &ctx.current_exe).map_err(
            |e| {
                tracing::error!(step = step.name, "post-hook FAILED: {}", e);
                e
            },
        )?;
    }

    tracing::info!("Step completed");

    Ok(())
}

/// Clean up a worktree after a workflow run.
///
/// Per the spec:
/// - If there are uncommitted changes, error and LEAVE the worktree.
/// - If there are unpushed commits, push them. Error if push fails (leave worktree).
/// - If clean and pushed, remove the worktree and delete its branch.
fn cleanup_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    token: &str,
    current_exe: &Path,
    default_branch: &str,
) -> Result<()> {
    // Check for uncommitted changes.
    if git::has_uncommitted_changes(worktree_path, token, current_exe)? {
        anyhow::bail!(
            "worktree has uncommitted changes — leaving worktree at {} for manual inspection",
            worktree_path.display()
        );
    }

    // Check for unpushed commits.
    if git::has_unpushed_commits(worktree_path, default_branch, token, current_exe)? {
        git::push_commits(worktree_path, token, current_exe)?;
    }

    // Safe to remove — clean and pushed.
    git::remove_worktree(repo_path, worktree_path, branch_name, token, current_exe)
}

#[cfg(test)]
mod tests {
    #[test]
    fn branch_name_sanitizes_slashes_and_hashes() {
        // Event labels like "acme/project#42" must become valid git branch names.
        let label = "acme/project#42";
        let sanitized = label.replace(['/', '#'], "-");
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('#'));
        assert_eq!(sanitized, "acme-project-42");
    }

    #[test]
    fn branch_name_format_is_valid() {
        // Verify the branch name format "ao/<label>-<ts>" produces valid git branch names.
        let label = "owner-repo-42";
        let ts: i64 = 1746475200; // deterministic timestamp for test
        let branch = format!("ao/{}-{}", label, ts);
        assert!(branch.starts_with("ao/"));
        assert_eq!(branch, "ao/owner-repo-42-1746475200");
        // Git branch names cannot contain spaces, control chars, or certain special chars.
        // The ao/ prefix and sanitized label + timestamp should be safe.
        assert!(!branch.contains(' '));
    }
}
