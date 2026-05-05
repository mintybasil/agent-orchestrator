use crate::askpass;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::instrument;

/// Ensure a git repository clone exists at `<data_root>/<owner>/<repo>/repo`.
///
/// If the directory does not exist, clones the repository. If it already
/// exists, pulls the latest changes from `origin <default_branch>`.
///
/// Authentication is handled via `GIT_ASKPASS`: the binary re-invokes itself
/// as a credential helper, reading the token from `AO_GIT_TOKEN`. The token
/// is never embedded in URLs or written to `.git/config`.
///
/// Returns the path to the repo directory.
pub fn ensure_repo(
    data_root: &Path,
    owner: &str,
    repo: &str,
    default_branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<PathBuf> {
    let repo_path = data_root.join(owner).join(repo).join("repo");

    if repo_path.exists() {
        pull_default_branch(&repo_path, default_branch, token, current_exe)?;
    } else {
        clone_repo(owner, repo, token, current_exe, &repo_path)?;
    }

    Ok(repo_path)
}

/// Build a git command with ASKPASS authentication env vars set.
///
/// The env vars are scoped to this child process only — they do not leak
/// into the parent's environment.
pub(crate) fn git_command(token: &str, current_exe: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env(askpass::ASKPASS_MODE_ENV, "1")
        .env(askpass::GIT_TOKEN_ENV, token)
        .env("GIT_ASKPASS", current_exe)
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// Clone a GitHub repository into `target_dir`.
#[instrument(skip(token, current_exe))]
fn clone_repo(
    owner: &str,
    repo: &str,
    token: &str,
    current_exe: &Path,
    target_dir: &Path,
) -> anyhow::Result<()> {
    let url = format!("https://github.com/{}/{}.git", owner, repo);

    tracing::info!("Cloning repo...");

    // Ensure parent directory exists
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = git_command(token, current_exe)
        .args(["clone", "--quiet", &url])
        .arg(target_dir)
        .output()
        .context("failed to spawn git clone")?;

    if output.status.success() {
        tracing::info!("Clone completed.");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let scrubbed = scrub_credentials(&stderr);
        anyhow::bail!(
            "git clone failed for {}/{} (exit code {:?}): {}",
            owner,
            repo,
            output.status.code(),
            scrubbed
        );
    }
}

/// Pull the latest changes from `origin <default_branch>` in the given repo directory.
///
/// Pull failure is non-fatal (logged as warning) — the repo might be on
/// a feature branch or have local changes.
#[instrument(skip(token, current_exe))]
fn pull_default_branch(
    repo_path: &Path,
    default_branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<()> {
    tracing::info!("Pulling latest changes...");

    let output = git_command(token, current_exe)
        .args(["pull", "--quiet", "origin", default_branch])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git pull")?;

    if output.status.success() {
        tracing::info!("Pull complete");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let scrubbed = scrub_credentials(&stderr);
        tracing::warn!(
            "git pull failed (exit code {:?}): {}; proceeding with existing checkout",
            output.status.code(),
            scrubbed
        );
        Ok(())
    }
}

/// Create a git worktree at `worktree_path` based on `default_branch`.
///
/// The worktree is created on a new unique branch (`branch_name`) starting
/// from `default_branch`. This avoids the git restriction that prevents a
/// worktree from sharing the same branch as the main clone.
///
/// Returns the name of the created branch so it can be cleaned up later.
#[instrument(skip(token, current_exe))]
pub fn create_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    default_branch: &str,
    branch_name: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<String> {
    tracing::info!(
        path = %worktree_path.display(),
        branch = branch_name,
        "Creating worktree..."
    );

    // Ensure parent directory exists
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = git_command(token, current_exe)
        .args(["worktree", "add", "-b", branch_name])
        .arg(worktree_path)
        .arg(default_branch)
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git worktree add")?;

    if output.status.success() {
        tracing::info!(branch = branch_name, "Worktree created");
        Ok(branch_name.to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let scrubbed = scrub_credentials(&stderr);
        anyhow::bail!(
            "git worktree add failed (exit code {:?}): {}",
            output.status.code(),
            scrubbed
        );
    }
}

/// Remove a git worktree at the given path and delete its associated branch.
///
/// Runs `git worktree remove` from the main repository, then prunes the
/// branch that was created for the worktree. Forces removal even if there
/// are uncommitted changes.
#[instrument(skip(token, current_exe))]
pub fn remove_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<()> {
    tracing::info!(path = %worktree_path.display(), "Removing worktree...");

    let output = git_command(token, current_exe)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git worktree remove")?;

    if output.status.success() {
        tracing::info!("Worktree removed");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let scrubbed = scrub_credentials(&stderr);
        anyhow::bail!(
            "git worktree remove failed (exit code {:?}): {}",
            output.status.code(),
            scrubbed
        );
    }

    // Delete the branch now that the worktree is gone.
    let branch_output = git_command(token, current_exe)
        .args(["branch", "-D", branch_name])
        .current_dir(repo_path)
        .output()
        .context("failed to spawn git branch -D")?;

    if branch_output.status.success() {
        tracing::debug!(branch = branch_name, "Branch deleted");
    } else {
        // Non-fatal: branch may have been deleted by a prior cleanup attempt
        // or renamed by the agent during the workflow run.
        let stderr = String::from_utf8_lossy(&branch_output.stderr);
        tracing::warn!(
            branch = branch_name,
            error = %scrub_credentials(&stderr),
            "Failed to delete worktree branch"
        );
    }

    Ok(())
}

/// Check for uncommitted changes in the given directory.
///
/// Returns `Ok(true)` if there are uncommitted changes, `Ok(false)` if clean.
pub fn has_uncommitted_changes(
    work_dir: &Path,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<bool> {
    let output = git_command(token, current_exe)
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .current_dir(work_dir)
        .output()
        .context("failed to spawn git diff-index")?;

    // git diff-index exits 0 if clean, 1 if there are differences
    Ok(!output.status.success())
}

/// Check for unpushed commits in the given directory.
///
/// Returns `Ok(true)` if there are local commits not on the remote,
/// `Ok(false)` if up-to-date.
pub fn has_unpushed_commits(
    work_dir: &Path,
    default_branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<bool> {
    let upstream = format!("origin/{}", default_branch);
    let output = git_command(token, current_exe)
        .args(["log", &upstream, "..HEAD", "--oneline"])
        .current_dir(work_dir)
        .output()
        .context("failed to spawn git log")?;

    Ok(output.status.success() && !output.stdout.is_empty())
}

/// Push commits from the given directory to the remote.
pub fn push_commits(work_dir: &Path, token: &str, current_exe: &Path) -> anyhow::Result<()> {
    tracing::info!("Pushing unpushed commits...");

    let output = git_command(token, current_exe)
        .args(["push"])
        .current_dir(work_dir)
        .output()
        .context("failed to spawn git push")?;

    if output.status.success() {
        tracing::info!("Push succeeded");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let scrubbed = scrub_credentials(&stderr);
        anyhow::bail!(
            "git push failed (exit code {:?}): {}",
            output.status.code(),
            scrubbed
        );
    }
}

/// Strip GitHub token patterns from a string before logging.
///
/// Defense-in-depth: git shouldn't include credentials in stderr when using
/// ASKPASS, but future git versions or edge cases could change that.
fn scrub_credentials(s: &str) -> String {
    let re = regex::Regex::new(r"(ghp_|gho_|ghs_|github_pat_)[A-Za-z0-9_-]+").unwrap();
    re.replace_all(s, "${1}***").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_format() {
        let url = format!("https://github.com/{}/{}.git", "zerokrab", "bento-hancho");
        assert_eq!(url, "https://github.com/zerokrab/bento-hancho.git");
    }

    #[test]
    fn repo_path_is_under_owner_repo() {
        let data_root = PathBuf::from("/tmp/test-data");
        let repo_path = data_root.join("zerokrab").join("bento-hancho").join("repo");
        assert!(repo_path.starts_with(&data_root));
        assert!(repo_path.to_string_lossy().contains("repo"));
    }

    #[test]
    fn scrub_credentials_masks_github_tokens() {
        let input = "Authentication failed for ghp_ABC123xyz token";
        let output = scrub_credentials(input);
        assert_eq!(output, "Authentication failed for ghp_*** token");
    }

    #[test]
    fn scrub_credentials_masks_github_pat() {
        let input = "Auth failed for github_pat_abc123DEF456";
        let output = scrub_credentials(input);
        assert_eq!(output, "Auth failed for github_pat_***");
    }

    #[test]
    fn scrub_credentials_preserves_non_token_text() {
        let input = "fatal: repository not found";
        assert_eq!(scrub_credentials(input), input);
    }

    #[test]
    fn parse_owner_repo_from_url() {
        let url = "https://github.com/zerokrab/bento-hancho.git";
        let path = url
            .rsplit_once("github.com/")
            .map(|(_, p)| p.trim_end_matches(".git"))
            .unwrap_or(url);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        assert_eq!(parts[0], "zerokrab");
        assert_eq!(parts[1], "bento-hancho");
    }
}
