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
#[instrument(skip(token, current_exe), parent = None)]
fn clone_repo(
    owner: &str,
    repo: &str,
    token: &str,
    current_exe: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    let url = format!("https://github.com/{}/{}.git", owner, repo);

    tracing::info!("Cloning repo...");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = git_command(token, current_exe)
        .args(["clone", "--quiet", &url])
        .arg(path)
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
#[instrument(skip(token, current_exe), parent = None)]
fn pull_default_branch(
    repo: &Path,
    base: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<()> {
    tracing::info!("Pulling latest changes...");

    let output = git_command(token, current_exe)
        .args(["pull", "--quiet", "origin", base])
        .current_dir(repo)
        .output()
        .context("failed to spawn git pull")?;

    if output.status.success() {
        tracing::info!("Pull complete");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            exit_code = output.status.code(),
            error = %scrub_credentials(&stderr),
            "git pull failed; proceeding with existing checkout"
        );
        Ok(())
    }
}

/// Check whether a repository has any commits (i.e. has an unborn HEAD).
///
/// Returns `true` if the repo is empty (no commits on any branch).
fn is_repo_empty(repo: &Path, token: &str, current_exe: &Path) -> bool {
    let output = git_command(token, current_exe)
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(repo)
        .output();

    match output {
        Ok(o) => !o.status.success(),
        Err(_) => true,
    }
}

/// Create an initial commit in an empty repository so that branches
/// can be created. Without at least one commit, `git worktree add -b
/// <branch> <path> <base>` fails because `base` is not a valid ref.
///
/// Uses `--allow-empty` so no sentinel file is needed.
fn ensure_initial_commit(
    repo: &Path,
    default_branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<()> {
    tracing::info!("Repository is empty — creating initial commit");

    // Rename the current branch to the desired default branch.
    // In a freshly cloned empty repo the remote creates an orphan HEAD
    // that typically points to a branch called "main" (or "master"), but
    // the local ref may not exist yet.  `git checkout -B` forces the
    // branch name regardless.
    let checkout = git_command(token, current_exe)
        .args(["checkout", "-B", default_branch])
        .current_dir(repo)
        .output()
        .context("failed to spawn git checkout -B")?;

    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        anyhow::bail!(
            "git checkout -B {} failed (exit code {:?}): {}",
            default_branch,
            checkout.status.code(),
            scrub_credentials(&stderr)
        );
    }

    // Create an empty commit so the default branch has a valid ref.
    // No identity is set — if none is configured, git commit will fail
    // with a clear error, which is the correct behaviour.
    let commit = git_command(token, current_exe)
        .args([
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit by agent-orchestrator",
        ])
        .current_dir(repo)
        .output()
        .context("failed to spawn git commit")?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        anyhow::bail!(
            "git commit failed (exit code {:?}): {}",
            commit.status.code(),
            scrub_credentials(&stderr)
        );
    }

    // Push the initial commit so the remote also has the branch.
    let push = git_command(token, current_exe)
        .args(["push", "origin", default_branch])
        .current_dir(repo)
        .output()
        .context("failed to spawn git push")?;

    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr);
        tracing::warn!(
            error = %scrub_credentials(&stderr),
            "git push of initial commit failed; worktree will still work locally"
        );
        // Non-fatal: the local branch now exists so worktree creation will
        // succeed.  The agent's harness step may push later anyway.
    }

    tracing::info!("Initial commit created on branch '{}'", default_branch);
    Ok(())
}

/// Create a git worktree at `path` based on `base`.
///
/// The worktree is created on a new unique branch (`branch`) starting
/// from `base`. This avoids the git restriction that prevents a
/// worktree from sharing the same branch as the main clone.
///
/// If the repository is empty (no commits), an initial commit is created
/// on the default branch first so that the worktree has a valid ref to
/// start from.
///
/// Returns the name of the created branch so it can be cleaned up later.
#[instrument(skip(token, current_exe), parent = None)]
pub fn create_worktree(
    repo: &Path,
    path: &Path,
    base: &str,
    branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<String> {
    tracing::info!("Creating worktree...");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // If the repo is empty (no commits), we need to seed it with an
    // initial commit before we can create a worktree that references
    // `base` — otherwise git emits "fatal: invalid reference: <base>".
    if is_repo_empty(repo, token, current_exe) {
        ensure_initial_commit(repo, base, token, current_exe)?;
    }

    let output = git_command(token, current_exe)
        .args(["worktree", "add", "-b", branch])
        .arg(path)
        .arg(base)
        .current_dir(repo)
        .output()
        .context("failed to spawn git worktree add")?;

    if output.status.success() {
        tracing::info!("Worktree created");
        Ok(branch.to_string())
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
    repo: &Path,
    path: &Path,
    branch: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<()> {
    tracing::info!("Removing worktree...");

    let output = git_command(token, current_exe)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(repo)
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
        .args(["branch", "-D", branch])
        .current_dir(repo)
        .output()
        .context("failed to spawn git branch -D")?;

    if branch_output.status.success() {
        tracing::debug!("Branch deleted");
    } else {
        // Non-fatal: branch may have been deleted by a prior cleanup attempt
        // or renamed by the agent during the workflow run.
        let stderr = String::from_utf8_lossy(&branch_output.stderr);
        tracing::warn!(
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

    // --- Empty repo / worktree tests ---

    /// Helper: create a bare-ish repo with NO commits (simulates a freshly
    /// created GitHub repo that has been cloned via `git clone`).
    fn make_empty_repo(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init")
            .status;
        assert!(status.success(), "git init failed");
    }

    /// Helper: create a repo WITH one commit on `main`.
    fn make_repo_with_commit(dir: &Path, default_branch: &str) {
        let status = std::process::Command::new("git")
            .args(["init", "-b", default_branch])
            .current_dir(dir)
            .output()
            .expect("git init -b")
            .status;
        assert!(status.success(), "git init -b failed");

        std::fs::write(dir.join("README.md"), "# test").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--no-gpg-sign"])
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .current_dir(dir)
            .output()
            .expect("git commit");
    }

    // Dummy token / exe path for tests — git_command doesn't validate them;
    // GIT_ASKPASS won't be called for local operations.
    fn fake_token() -> String {
        "fake-test-token".to_string()
    }
    fn fake_exe() -> PathBuf {
        PathBuf::from("/usr/bin/true")
    }

    #[test]
    fn is_repo_empty_detects_empty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_empty_repo(tmp.path());
        assert!(is_repo_empty(tmp.path(), &fake_token(), &fake_exe()));
    }

    #[test]
    fn is_repo_empty_returns_false_for_nonempty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");
        assert!(!is_repo_empty(tmp.path(), &fake_token(), &fake_exe()));
    }

    #[test]
    fn create_worktree_succeeds_on_empty_repo() {
        // This is the core regression test for issue #84:
        // `git worktree add -b <branch> <path> main` fails on an empty repo
        // because `main` is not a valid ref yet.  The fix seeds an empty
        // initial commit (--allow-empty) before creating the worktree.
        let tmp = tempfile::tempdir().unwrap();
        make_empty_repo(tmp.path());

        // Set a local git identity in the test repo so that git commit can
        // succeed.  The production code intentionally does NOT set an identity
        // — if no identity is configured, the commit fails with a clear
        // error.  Tests must provide their own.
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(tmp.path())
            .output()
            .expect("git config user.email");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(tmp.path())
            .output()
            .expect("git config user.name");

        let wt_path = tmp.path().join("worktree-1");
        let branch = "ao/test-worktree-1";

        let result = create_worktree(
            tmp.path(),
            &wt_path,
            "main",
            branch,
            &fake_token(),
            &fake_exe(),
        );

        assert!(
            result.is_ok(),
            "create_worktree failed on empty repo: {:?}",
            result.err()
        );
        assert!(wt_path.exists(), "worktree directory was not created");
        // The initial commit is empty (no .gitkeep), so we just verify the
        // repo is no longer considered empty.
        assert!(
            !is_repo_empty(tmp.path(), &fake_token(), &fake_exe()),
            "repo should not be empty after ensure_initial_commit"
        );

        // Clean up the worktree so the temp dir can be removed.
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_path)
            .current_dir(tmp.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(tmp.path())
            .output();
    }

    #[test]
    fn create_worktree_succeeds_on_nonempty_repo() {
        // Sanity check that existing functionality still works.
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");

        let wt_path = tmp.path().join("worktree-1");
        let branch = "ao/test-worktree-2";

        let result = create_worktree(
            tmp.path(),
            &wt_path,
            "main",
            branch,
            &fake_token(),
            &fake_exe(),
        );

        assert!(
            result.is_ok(),
            "create_worktree failed on non-empty repo: {:?}",
            result.err()
        );
        assert!(wt_path.exists(), "worktree directory was not created");
        assert!(
            wt_path.join("README.md").exists(),
            "README.md not found in worktree"
        );

        // Clean up.
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_path)
            .current_dir(tmp.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(tmp.path())
            .output();
    }
}
