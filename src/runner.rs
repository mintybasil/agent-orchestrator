use crate::config::GitConfig;
use crate::git;
use crate::hooks;
use crate::template::render;
// Re-export EventKey so consumers that imported from runner still compile.
pub use crate::trigger::EventKey;
use crate::workflow::Step;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::instrument;

struct StepContext {
    error_path: PathBuf,
    show_logs: bool,
    log_path: PathBuf,
    /// Path to write the rendered prompt for auditing/debugging.
    prompt_path: PathBuf,
    /// The directory from which the harness should be invoked.
    /// This may be the worktree, the repo, or the event workspace.
    work_dir: PathBuf,
    key: String,
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
#[instrument(skip_all, fields(key=key.to_string()))]
pub async fn run_workflow(
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
        .join(&key.workspace_id);
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
        let branch = format!(
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
            &branch,
            token,
            current_exe,
        )?;
        Some((wt_path, branch))
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
        ("default_branch".into(), git_config.default_branch.clone()),
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
        show_logs,
    };

    let result = run_steps(steps, &ctx).await;

    // Clean up worktree if one was created.
    if let Some((ref wt_path, ref branch)) = worktree_info {
        let repo = repo_dir.as_ref().unwrap();
        if let Err(cleanup_err) = cleanup_worktree(
            repo,
            wt_path,
            branch,
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
        let sanitized_name = step.name.replace(' ', "-");
        let error_path = ctx
            .workspace_dir
            .join(format!("step_{:02}_{}.error", idx, sanitized_name));
        let log_path = ctx
            .workspace_dir
            .join(format!("step_{:02}_{}.log", idx, sanitized_name));
        let prompt_path = ctx
            .workspace_dir
            .join(format!("step_{:02}_{}.prompt", idx, sanitized_name));

        let step_ctx = StepContext {
            error_path,
            show_logs: ctx.show_logs,
            log_path,
            prompt_path,
            work_dir: ctx.harness_work_dir.clone(),
            key: ctx.key.clone(),
        };

        run_step(step, &step_ctx, &ctx.vars).await?
    }

    Ok(())
}

#[instrument(skip_all, fields(step=step.name, key = ctx.key), parent = None)]
async fn run_step(step: &Step, ctx: &StepContext, vars: &HashMap<String, String>) -> Result<()> {
    tracing::info!("Starting step");
    // --- Pre-hooks -----------------------------------------------------------
    for hook in &step.pre_hooks {
        hooks::run_hook(hook, vars, &ctx.error_path).map_err(|e| {
            tracing::error!("pre-hook FAILED: {}", e);
            e
        })?;
    }

    let harness = step.harness.build();
    let rendered_prompt = render(&step.prompt_template, vars);

    // Write the rendered prompt to the workspace for auditing/debugging.
    fs::write(&ctx.prompt_path, &rendered_prompt)
        .map_err(|e| anyhow::anyhow!("failed to write prompt log: {}", e))?;

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
        hooks::run_hook(hook, vars, &ctx.error_path).map_err(|e| {
            tracing::error!(step = step.name, "post-hook FAILED: {}", e);
            e
        })?;
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
    branch: &str,
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
    git::remove_worktree(repo_path, worktree_path, branch, token, current_exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    #[test]
    fn step_filenames_replace_spaces_with_hyphens() {
        let workspace_dir = PathBuf::from("/tmp/workspace");
        let step_name = "Address Review";
        let sanitized_name = step_name.replace(' ', "-");
        let idx = 0;

        let log_path = workspace_dir.join(format!("step_{:02}_{}.log", idx, sanitized_name));
        let error_path = workspace_dir.join(format!("step_{:02}_{}.error", idx, sanitized_name));
        let prompt_path = workspace_dir.join(format!("step_{:02}_{}.prompt", idx, sanitized_name));

        assert_eq!(
            log_path,
            PathBuf::from("/tmp/workspace/step_00_Address-Review.log")
        );
        assert_eq!(
            error_path,
            PathBuf::from("/tmp/workspace/step_00_Address-Review.error")
        );
        assert_eq!(
            prompt_path,
            PathBuf::from("/tmp/workspace/step_00_Address-Review.prompt")
        );
    }

    #[test]
    fn step_filenames_without_spaces_unchanged() {
        let workspace_dir = PathBuf::from("/tmp/workspace");
        let step_name = "triage";
        let sanitized_name = step_name.replace(' ', "-");
        let idx = 0;

        let log_path = workspace_dir.join(format!("step_{:02}_{}.log", idx, sanitized_name));

        assert_eq!(log_path, PathBuf::from("/tmp/workspace/step_00_triage.log"));
    }

    #[test]
    fn step_filenames_multiple_spaces_replaced() {
        let workspace_dir = PathBuf::from("/tmp/workspace");
        let step_name = "Fix Bug And Test";
        let sanitized_name = step_name.replace(' ', "-");
        let idx = 1;

        let log_path = workspace_dir.join(format!("step_{:02}_{}.log", idx, sanitized_name));

        assert_eq!(
            log_path,
            PathBuf::from("/tmp/workspace/step_01_Fix-Bug-And-Test.log")
        );
    }

    #[test]
    fn prompt_path_follows_step_naming_convention() {
        // Verify that prompt_path uses the same naming pattern as error_path and log_path.
        let workspace_dir = PathBuf::from("/tmp/workspace");
        let prompt_path = workspace_dir.join(format!("step_{:02}_{}.prompt", 0, "triage"));
        assert_eq!(
            prompt_path,
            PathBuf::from("/tmp/workspace/step_00_triage.prompt")
        );
    }

    #[test]
    fn prompt_file_is_written_with_rendered_content() {
        // Verify that fs::write to prompt_path stores the rendered prompt content.
        let tmp = tempfile::tempdir().unwrap();
        let prompt_path = tmp.path().join("step_00_triage.prompt");
        let rendered = "Triage issue #42 for acme/project.";
        fs::write(&prompt_path, rendered).unwrap();
        let read_back = fs::read_to_string(&prompt_path).unwrap();
        assert_eq!(read_back, rendered);
    }

    #[test]
    fn render_with_template_vars_matches_prompt_file() {
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("owner".to_string(), "acme".to_string());
        vars.insert("repo".to_string(), "project".to_string());
        vars.insert("issue_number".to_string(), "42".to_string());

        let template = "Triage {{owner}}/{{repo}}#{{issue_number}}";
        let rendered = render(template, &vars);

        let tmp = tempfile::tempdir().unwrap();
        let prompt_path = tmp.path().join("step_00_triage.prompt");
        fs::write(&prompt_path, &rendered).unwrap();

        let read_back = fs::read_to_string(&prompt_path).unwrap();
        assert_eq!(read_back, "Triage acme/project#42");
    }

    #[test]
    fn workspace_dir_uses_workspace_id_for_issues() {
        // For issues, workspace_id is just the number string.
        let key = EventKey {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            number: 42,
            workspace_id: "42".to_string(),
            label: "acme/project#42".to_string(),
            variables: HashMap::new(),
        };
        let data_root = PathBuf::from("/tmp/data");
        let workspace_dir = data_root
            .join(&key.owner)
            .join(&key.repo)
            .join(&key.workspace_id);
        assert_eq!(workspace_dir, PathBuf::from("/tmp/data/acme/project/42"));
    }

    #[test]
    fn workspace_dir_uses_workspace_id_for_pr_reviews() {
        // For PR reviews, workspace_id includes the review ID for uniqueness.
        let key = EventKey {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            number: 99,
            workspace_id: "99_review-1234567".to_string(),
            label: "acme/project#99_review-1234567".to_string(),
            variables: HashMap::from([("pr_number".to_string(), "99".to_string())]),
        };
        let data_root = PathBuf::from("/tmp/data");
        let workspace_dir = data_root
            .join(&key.owner)
            .join(&key.repo)
            .join(&key.workspace_id);
        // Each PR review gets its own discrete directory.
        assert_eq!(
            workspace_dir,
            PathBuf::from("/tmp/data/acme/project/99_review-1234567")
        );
        // Verify that two reviews on the same PR don't share a directory.
        let key2 = EventKey {
            owner: "acme".to_string(),
            repo: "project".to_string(),
            number: 99,
            workspace_id: "99_review-7654321".to_string(),
            label: "acme/project#99_review-7654321".to_string(),
            variables: HashMap::from([("pr_number".to_string(), "99".to_string())]),
        };
        let workspace_dir2 = data_root
            .join(&key2.owner)
            .join(&key2.repo)
            .join(&key2.workspace_id);
        assert_ne!(workspace_dir, workspace_dir2);
    }
}
