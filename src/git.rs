use anyhow::Context;
use git2::{
    BranchType, FetchOptions, RemoteCallbacks, Repository, Signature, WorktreeAddOptions,
    WorktreePruneOptions,
};
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Ensure a git repository clone exists at `<data_root>/<owner>/<repo>/repo`.
///
/// If the directory does not exist, clones the repository. If it already
/// exists, pulls the latest changes from `origin <default_branch>`.
///
/// Authentication is handled via `git2::RemoteCallbacks` that supply the token
/// for HTTPS operations. The token is never embedded in URLs or written to
/// `.git/config`.
///
/// Returns the path to the repo directory.
pub fn ensure_repo(
    data_root: &Path,
    owner: &str,
    repo: &str,
    default_branch: &str,
    token: &str,
) -> anyhow::Result<PathBuf> {
    let repo_path = data_root.join(owner).join(repo).join("repo");

    if repo_path.exists() {
        pull_default_branch(&repo_path, default_branch, token)?;
    } else {
        clone_repo(owner, repo, token, &repo_path)?;
    }

    Ok(repo_path)
}

/// Build `git2::RemoteCallbacks` that authenticate using the provided token.
///
/// For username prompts, returns `x-access-token`; for password prompts,
/// returns the token.
fn callbacks(token: &str) -> RemoteCallbacks<'_> {
    let mut cb = RemoteCallbacks::new();
    let token = token.to_string();
    cb.credentials(move |_url, username_from_url, _allowed_types| {
        let user = username_from_url
            .map(|u| u.to_string())
            .unwrap_or_else(|| "x-access-token".to_string());
        git2::Cred::userpass_plaintext(&user, &token)
    });
    cb
}

/// Build `FetchOptions` with authentication callbacks and prune setting.
fn fetch_options(token: &str) -> FetchOptions<'_> {
    let mut opts = FetchOptions::new();
    opts.remote_callbacks(callbacks(token));
    opts.prune(git2::FetchPrune::On);
    opts
}

/// Clone a GitHub repository into `target_dir`.
#[instrument(skip(token), parent = None)]
fn clone_repo(owner: &str, repo: &str, token: &str, path: &Path) -> anyhow::Result<()> {
    let url = format!("https://github.com/{}/{}.git", owner, repo);

    tracing::info!("Cloning repo...");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_options(token));

    builder.clone(&url, path).map_err(|e| {
        anyhow::anyhow!(
            "git clone failed for {}/{}: {}",
            owner,
            repo,
            scrub_credentials(&e.to_string())
        )
    })?;

    tracing::info!("Clone completed.");
    Ok(())
}

/// Pull the latest changes from `origin <default_branch>` in the given repo directory.
///
/// Pull failure is non-fatal (logged as warning) — the repo might be on
/// a feature branch or have local changes.
#[instrument(skip(token), parent = None)]
fn pull_default_branch(repo_path: &Path, base: &str, token: &str) -> anyhow::Result<()> {
    tracing::info!("Pulling latest changes...");

    let repo = match Repository::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to open repo for pull; proceeding with existing checkout");
            return Ok(());
        }
    };

    // Find the remote named "origin"
    let mut remote = match repo.find_remote("origin") {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to find remote 'origin'; proceeding with existing checkout");
            return Ok(());
        }
    };

    // Fetch from origin
    if let Err(e) = remote.fetch(&[base], Some(&mut fetch_options(token)), None) {
        tracing::warn!(
            error = %scrub_credentials(&e.to_string()),
            "git fetch failed; proceeding with existing checkout"
        );
        return Ok(());
    }

    // Look up the fetch head to find what we just fetched
    let fetch_head = match repo.find_reference("FETCH_HEAD") {
        Ok(fh) => fh,
        Err(e) => {
            tracing::warn!(error = %e, "FETCH_HEAD not found after fetch; proceeding with existing checkout");
            return Ok(());
        }
    };

    let fetch_head_oid = match fetch_head.target() {
        Some(oid) => oid,
        None => {
            tracing::warn!("FETCH_HEAD has no target; proceeding with existing checkout");
            return Ok(());
        }
    };

    let fetch_commit = match repo.find_commit(fetch_head_oid) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to look up fetch commit; proceeding with existing checkout");
            return Ok(());
        }
    };

    // Try to fast-forward the local branch to the fetched commit.
    let branch_ref_name = format!("refs/heads/{}", base);
    match repo.find_reference(&branch_ref_name) {
        Ok(mut local_ref) => {
            if let Err(e) = local_ref.set_target(fetch_head_oid, "pull: fast-forward") {
                tracing::warn!(
                    error = %scrub_credentials(&e.to_string()),
                    "Failed to update local branch; proceeding with existing checkout"
                );
                return Ok(());
            }

            // Update the working tree to match the new HEAD
            if let Err(e) = repo.set_head(&branch_ref_name) {
                tracing::warn!(error = %e, "Failed to set HEAD; proceeding with existing checkout");
                return Ok(());
            }

            if let Err(e) = repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force())) {
                tracing::warn!(error = %e, "Failed to checkout HEAD; proceeding with existing checkout");
                return Ok(());
            }
        }
        Err(_) => {
            // Local branch doesn't exist yet; create it from the fetch.
            if let Err(e) = repo.branch(base, &fetch_commit, false) {
                tracing::warn!(
                    error = %scrub_credentials(&e.to_string()),
                    "Failed to create local branch; proceeding with existing checkout"
                );
                return Ok(());
            }
        }
    }

    tracing::info!("Pull complete");
    Ok(())
}

/// Check whether a repository has any commits (i.e. has an unborn HEAD).
///
/// Returns `true` if the repo is empty (no commits on any branch).
fn is_repo_empty(repo_path: &Path) -> bool {
    let repo = match Repository::open(repo_path) {
        Ok(r) => r,
        Err(_) => return true,
    };

    // repo.head() fails if there's no HEAD reference (empty repo)
    // or if HEAD points to an unborn branch with no commits.
    match repo.head() {
        Ok(head) => {
            // HEAD exists, but check if the target commit exists
            head.target().is_none() || repo.head().unwrap().peel_to_commit().is_err()
        }
        Err(_) => true,
    }
}

/// Create an initial commit in an empty repository so that branches
/// can be created. Without at least one commit, you can't create a branch
/// because there's no valid ref to start from.
///
/// Uses an empty tree so no sentinel file is needed.
fn ensure_initial_commit(
    repo_path: &Path,
    default_branch: &str,
    token: &str,
) -> anyhow::Result<()> {
    tracing::info!("Repository is empty — creating initial commit");

    let repo =
        Repository::open(repo_path).context("failed to open repository for initial commit")?;

    // Create a signature for the commit.
    let sig = Signature::now(
        "agent-orchestrator",
        "agent-orchestrator@users.noreply.github.com",
    )
    .context("failed to create git signature for initial commit")?;

    // Create an empty tree (no files needed)
    let tree_id = repo.treebuilder(None)?.write()?;
    let tree = repo.find_tree(tree_id)?;

    // Determine if there's already a HEAD and create/update the commit.
    let head_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());

    // Create the empty commit
    let commit_id = match head_commit {
        Some(parent) => repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Initial commit by agent-orchestrator",
            &tree,
            &[&parent],
        )?,
        None => {
            // No parent — first commit in the repo
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Initial commit by agent-orchestrator",
                &tree,
                &[],
            )?
        }
    };

    // Ensure the default branch ref exists.
    // If HEAD still points to a differently-named branch, create the ref.
    let branch_refname = format!("refs/heads/{}", default_branch);
    if repo.find_reference(&branch_refname).is_err() {
        // The default branch ref doesn't exist yet — create it pointing to our commit.
        repo.reference(&branch_refname, commit_id, false, "create default branch")?;
        repo.set_head(&branch_refname)?;
    }

    // Push the initial commit so the remote also has the branch (non-fatal if it fails).
    push_branch(&repo, default_branch, token)?;

    tracing::info!("Initial commit created on branch '{}'", default_branch);
    Ok(())
}

/// Push a branch to origin (used by ensure_initial_commit).
/// Failures are non-fatal (logged as warnings).
fn push_branch(repo: &Repository, branch: &str, token: &str) -> anyhow::Result<()> {
    let mut remote = match repo.find_remote("origin") {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to find remote 'origin' for push of initial commit");
            return Ok(());
        }
    };

    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    if let Err(e) = remote.push(&[&refspec], Some(&mut push_options(token))) {
        tracing::warn!(
            error = %scrub_credentials(&e.to_string()),
            "git push of initial commit failed; worktree will still work locally"
        );
    }

    Ok(())
}

/// Build push options with authentication callbacks.
fn push_options(token: &str) -> git2::PushOptions<'_> {
    let mut opts = git2::PushOptions::new();
    opts.remote_callbacks(callbacks(token));
    opts
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
#[instrument(skip(token), parent = None)]
pub fn create_worktree(
    repo_path: &Path,
    path: &Path,
    base: &str,
    branch: &str,
    token: &str,
) -> anyhow::Result<String> {
    tracing::info!("Creating worktree...");

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // If the repo is empty (no commits), we need to seed it with an
    // initial commit before we can create a worktree that references
    // `base` — otherwise there's no valid ref to branch from.
    if is_repo_empty(repo_path) {
        ensure_initial_commit(repo_path, base, token)?;
    }

    let repo =
        Repository::open(repo_path).context("failed to open repository for worktree creation")?;

    // Look up the base branch reference.
    let base_ref = repo
        .find_reference(&format!("refs/heads/{}", base))
        .or_else(|_| repo.find_reference(&format!("refs/remotes/origin/{}", base)))
        .context(format!("failed to find base reference '{}'", base))?;

    // Create the new branch pointing to the base commit.
    let base_commit = base_ref.peel_to_commit().context(format!(
        "failed to peel base reference '{}' to commit",
        base
    ))?;

    repo.branch(branch, &base_commit, false).context(format!(
        "failed to create branch '{}' (may already exist)",
        branch
    ))?;

    // Look up the branch reference we just created (for the worktree add options).
    let branch_ref = repo
        .find_reference(&format!("refs/heads/{}", branch))
        .context(format!(
            "failed to find newly created branch reference '{}'",
            branch
        ))?;

    // Create the worktree for the new branch.
    // Use a sanitized worktree name (slashes replaced with hyphens) because
    // libgit2 stores worktree metadata under `.git/worktrees/<name>/` and
    // nested directory creation may not be handled properly.
    let wt_name = branch.replace('/', "-");
    let mut opts = WorktreeAddOptions::new();
    opts.reference(Some(&branch_ref));

    repo.worktree(&wt_name, path, Some(&opts))
        .context("failed to create worktree")?;

    tracing::info!("Worktree created");
    Ok(branch.to_string())
}

/// Remove a git worktree at the given path and delete its associated branch.
///
/// Forces removal even if there are uncommitted changes.
pub fn remove_worktree(repo_path: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    tracing::info!("Removing worktree...");

    let repo =
        Repository::open(repo_path).context("failed to open repository for worktree removal")?;

    // Find and prune the worktree. This removes the worktree directory
    // and cleans up the administrative state.
    // Use the sanitized name (slashes replaced with hyphens) matching how
    // the worktree was created in create_worktree.
    let wt_name = branch.replace('/', "-");
    let worktree = repo
        .find_worktree(&wt_name)
        .context(format!("failed to find worktree for branch '{}'", branch))?;

    let mut prune_opts = WorktreePruneOptions::new();
    prune_opts.valid(true).locked(true).working_tree(true);

    worktree
        .prune(Some(&mut prune_opts))
        .context("git worktree prune failed")?;

    tracing::info!("Worktree removed");

    // Delete the branch now that the worktree is gone.
    let branch_obj = repo.find_branch(branch, BranchType::Local);
    match branch_obj {
        Ok(mut b) => {
            if let Err(e) = b.delete() {
                // Non-fatal: branch may have been deleted by a prior cleanup attempt
                // or renamed by the agent during the workflow run.
                tracing::warn!(error = %e, "Failed to delete worktree branch");
            } else {
                tracing::debug!("Branch deleted");
            }
        }
        Err(e) => {
            // Non-fatal: branch may have been deleted by a prior cleanup attempt
            tracing::warn!(error = %e, "Failed to find worktree branch for deletion");
        }
    }

    // Also clean up the worktree path if it still exists (defense-in-depth).
    if path.exists()
        && let Err(e) = std::fs::remove_dir_all(path)
    {
        tracing::warn!(error = %e, "Failed to remove worktree directory after prune");
    }

    Ok(())
}

/// Check for uncommitted changes in the given directory.
///
/// Returns `Ok(true)` if there are uncommitted changes, `Ok(false)` if clean.
pub fn has_uncommitted_changes(work_dir: &Path) -> anyhow::Result<bool> {
    let repo = Repository::open(work_dir)
        .context("failed to open repository to check for uncommitted changes")?;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);

    let statuses = repo
        .statuses(Some(&mut opts))
        .context("failed to get repository status")?;

    Ok(!statuses.is_empty())
}

/// Strip GitHub token patterns from a string before logging.
///
/// Defense-in-depth: git2 shouldn't include credentials in error messages
/// when using callbacks, but edge cases could leak them.
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
        let input = "Auth failed for github_pat_ABC123xyz";
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

    /// Helper: create a repo WITH one commit on `default_branch`.
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

    fn fake_token() -> String {
        "fake-test-token".to_string()
    }

    #[test]
    fn is_repo_empty_detects_empty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_empty_repo(tmp.path());
        assert!(is_repo_empty(tmp.path()));
    }

    #[test]
    fn is_repo_empty_returns_false_for_nonempty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");
        assert!(!is_repo_empty(tmp.path()));
    }

    #[test]
    fn create_worktree_succeeds_on_empty_repo() {
        // This is the core regression test for issue #84:
        // Worktree creation fails on an empty repo because there's no valid
        // ref to start from. The fix seeds an empty initial commit before
        // creating the worktree.
        let tmp = tempfile::tempdir().unwrap();
        make_empty_repo(tmp.path());

        let wt_path = tmp.path().join("worktree-1");
        let branch = "ao/test-worktree-1";

        let result = create_worktree(tmp.path(), &wt_path, "main", branch, &fake_token());

        assert!(
            result.is_ok(),
            "create_worktree failed on empty repo: {:?}",
            result.err()
        );
        assert!(wt_path.exists(), "worktree directory was not created");
        // The initial commit is empty (no .gitkeep), so we just verify the
        // repo is no longer considered empty.
        assert!(
            !is_repo_empty(tmp.path()),
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

        let result = create_worktree(tmp.path(), &wt_path, "main", branch, &fake_token());

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

    #[test]
    fn has_uncommitted_changes_detects_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");

        // Add an untracked file
        std::fs::write(tmp.path().join("new_file.txt"), "hello").unwrap();

        assert!(has_uncommitted_changes(tmp.path()).unwrap());
    }

    #[test]
    fn has_uncommitted_changes_returns_false_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");

        assert!(!has_uncommitted_changes(tmp.path()).unwrap());
    }

    #[test]
    fn has_uncommitted_changes_detects_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo_with_commit(tmp.path(), "main");

        // Modify tracked file
        std::fs::write(tmp.path().join("README.md"), "# modified").unwrap();

        assert!(has_uncommitted_changes(tmp.path()).unwrap());
    }
}
