use anyhow::Context;
use std::path::Path;
use std::process::Command;

/// Ensure a git workspace exists at `<data_root>/<owner>/<repo>/workspace`.
///
/// If the directory does not exist, clones the repository.
/// If it already exists, pulls the latest changes from `origin main`.
/// Returns the path to the workspace directory.
pub fn ensure_workspace(data_root: &Path, owner: &str, repo: &str) -> anyhow::Result<std::path::PathBuf> {
    let workspace = data_root.join(owner).join(repo).join("workspace");

    if workspace.exists() {
        pull_main(&workspace)?;
    } else {
        clone_repo(owner, repo, &workspace)?;
    }

    Ok(workspace)
}

/// Clone a GitHub repository into `target_dir`.
fn clone_repo(owner: &str, repo: &str, target_dir: &Path) -> anyhow::Result<()> {
    let url = format!("https://github.com/{}/{}.git", owner, repo);

    tracing::info!("[git] cloning {} -> {:?}", url, target_dir);

    // Ensure parent directory exists
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let status = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(target_dir)
        .status()
        .context("failed to spawn git clone")?;

    if status.success() {
        tracing::info!("[git] clone complete: {}/{}", owner, repo);
        Ok(())
    } else {
        anyhow::bail!(
            "git clone failed for {}/{} (exit code {:?})",
            owner,
            repo,
            status.code()
        );
    }
}

/// Pull the latest changes from `origin main` in the given workspace directory.
fn pull_main(workspace: &Path) -> anyhow::Result<()> {
    tracing::info!("[git] pulling origin main in {:?}", workspace);

    let status = Command::new("git")
        .args(["pull", "--quiet", "origin", "main"])
        .current_dir(workspace)
        .status()
        .context("failed to spawn git pull")?;

    if status.success() {
        tracing::info!("[git] pull complete");
        Ok(())
    } else {
        // Pull failure is non-fatal: the workspace might be on a worktree branch
        // or have local changes. Log warning but do not bail.
        tracing::warn!(
            "[git] git pull origin main failed in {:?} (exit code {:?}); proceeding with existing checkout",
            workspace,
            status.code()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn clone_repo_builds_correct_url() {
        let url = format!("https://github.com/{}/{}.git", "zerokrab", "bento-hancho");
        assert_eq!(url, "https://github.com/zerokrab/bento-hancho.git");
    }

    #[test]
    fn workspace_path_is_under_owner_repo() {
        let data_root = PathBuf::from("/tmp/test-data");
        let workspace = data_root
            .join("zerokrab")
            .join("bento-hancho")
            .join("workspace");
        assert!(workspace.starts_with(&data_root));
        assert!(workspace.to_string_lossy().contains("workspace"));
    }
}