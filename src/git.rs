use anyhow::Context;
use std::path::Path;
use std::process::Command;

/// Ensure a git workspace exists at `<data_root>/<owner>/<repo>/workspace`.
///
/// If the directory does not exist, clones the repository using the provided
/// GitHub token for authentication. If it already exists, pulls the latest
/// changes from `origin main`.
///
/// Returns the path to the workspace directory.
pub fn ensure_workspace(
    data_root: &Path,
    owner: &str,
    repo: &str,
    token: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let workspace = data_root.join(owner).join(repo).join("workspace");

    if workspace.exists() {
        pull_main(&workspace, token)?;
    } else {
        clone_repo(owner, repo, token, &workspace)?;
    }

    Ok(workspace)
}

/// Build an authenticated GitHub URL embedding the token.
///
/// Uses the `x-access-token` username convention recommended by GitHub:
/// `https://x-access-token:<token>@github.com/<owner>/<repo>.git`
fn authenticated_url(owner: &str, repo: &str, token: &str) -> String {
    format!(
        "https://x-access-token:{}@github.com/{}/{}.git",
        token, owner, repo
    )
}

/// Clone a GitHub repository into `target_dir`.
fn clone_repo(owner: &str, repo: &str, token: &str, target_dir: &Path) -> anyhow::Result<()> {
    let url = authenticated_url(owner, repo, token);

    tracing::info!("[git] cloning {}/{} -> {:?}", owner, repo, target_dir);

    // Ensure parent directory exists
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let status = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(target_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
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
///
/// Reconfigures the remote URL to include the token for authentication,
/// then pulls. Pull failure is non-fatal (logged as warning).
fn pull_main(workspace: &Path, token: &str) -> anyhow::Result<()> {
    tracing::info!("[git] pulling origin main in {:?}", workspace);

    // Determine owner/repo from the current remote URL to rebuild it with the token.
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("failed to spawn git remote get-url")?;

    let current_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Extract owner/repo from the URL (strip auth prefix and .git suffix).
    let path = current_url
        .rsplit_once("github.com/")
        .map(|(_, path)| path.trim_end_matches(".git"))
        .unwrap_or(&current_url);

    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("cannot parse owner/repo from remote URL: {}", current_url);
    }
    let owner = parts[0];
    let repo = parts[1];

    let authed_url = authenticated_url(owner, repo, token);

    // Set the remote URL with the token embedded for this pull.
    let set_url_status = Command::new("git")
        .args(["remote", "set-url", "origin", &authed_url])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .context("failed to spawn git remote set-url")?;

    if !set_url_status.success() {
        anyhow::bail!("git remote set-url failed in {:?}", workspace);
    }

    let pull_status = Command::new("git")
        .args(["pull", "--quiet", "origin", "main"])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .context("failed to spawn git pull")?;

    // Strip the token from the remote URL for safety.
    let public_url = format!("https://github.com/{}/{}.git", owner, repo);
    let _ = Command::new("git")
        .args(["remote", "set-url", "origin", &public_url])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status();

    if pull_status.success() {
        tracing::info!("[git] pull complete");
        Ok(())
    } else {
        // Pull failure is non-fatal: the workspace might be on a worktree branch
        // or have local changes. Log warning but do not bail.
        tracing::warn!(
            "[git] git pull origin main failed in {:?} (exit code {:?}); proceeding with existing checkout",
            workspace,
            pull_status.code()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_url_format() {
        let url = authenticated_url("zerokrab", "bento-hancho", "ghp_test123");
        assert_eq!(
            url,
            "https://x-access-token:ghp_test123@github.com/zerokrab/bento-hancho.git"
        );
    }

    #[test]
    fn workspace_path_is_under_owner_repo() {
        use std::path::PathBuf;
        let data_root = PathBuf::from("/tmp/test-data");
        let workspace = data_root
            .join("zerokrab")
            .join("bento-hancho")
            .join("workspace");
        assert!(workspace.starts_with(&data_root));
        assert!(workspace.to_string_lossy().contains("workspace"));
    }

    #[test]
    fn parse_owner_repo_from_public_url() {
        let url = "https://github.com/zerokrab/bento-hancho.git";
        let path = url
            .rsplit_once("github.com/")
            .map(|(_, path)| path.trim_end_matches(".git"))
            .unwrap_or(url);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        assert_eq!(parts[0], "zerokrab");
        assert_eq!(parts[1], "bento-hancho");
    }

    #[test]
    fn parse_owner_repo_from_authed_url() {
        let url = "https://x-access-token:ghp_secret@github.com/zerokrab/bento-hancho.git";
        let path = url
            .rsplit_once("github.com/")
            .map(|(_, path)| path.trim_end_matches(".git"))
            .unwrap_or(url);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        assert_eq!(parts[0], "zerokrab");
        assert_eq!(parts[1], "bento-hancho");
    }
}
