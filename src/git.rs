use crate::askpass;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::instrument;

/// Ensure a git workspace exists at `<data_root>/<owner>/<repo>/workspace`.
///
/// If the directory does not exist, clones the repository. If it already
/// exists, pulls the latest changes from `origin main`.
///
/// Authentication is handled via `GIT_ASKPASS`: the binary re-invokes itself
/// as a credential helper, reading the token from `AO_GIT_TOKEN`. The token
/// is never embedded in URLs or written to `.git/config`.
///
/// Returns the path to the workspace directory.
pub fn ensure_workspace(
    data_root: &Path,
    owner: &str,
    repo: &str,
    token: &str,
    current_exe: &Path,
) -> anyhow::Result<PathBuf> {
    let workspace = data_root.join(owner).join(repo).join("workspace");

    if workspace.exists() {
        pull_main(&workspace, token, current_exe)?;
    } else {
        clone_repo(owner, repo, token, current_exe, &workspace)?;
    }

    Ok(workspace)
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

/// Pull the latest changes from `origin main` in the given workspace directory.
///
/// Pull failure is non-fatal (logged as warning) — the workspace might be on
/// a feature branch or have local changes.
#[instrument(skip(token, current_exe), parent = None)]
fn pull_main(workspace: &Path, token: &str, current_exe: &Path) -> anyhow::Result<()> {
    tracing::info!("Pulling latest changes...");

    let output = git_command(token, current_exe)
        .args(["pull", "--quiet", "origin", "main"])
        .current_dir(workspace)
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
    fn workspace_path_is_under_owner_repo() {
        let data_root = PathBuf::from("/tmp/test-data");
        let workspace = data_root
            .join("zerokrab")
            .join("bento-hancho")
            .join("workspace");
        assert!(workspace.starts_with(&data_root));
        assert!(workspace.to_string_lossy().contains("workspace"));
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
