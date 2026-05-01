//! Hook system — pre/post step hooks.
//!
//! Hooks are small checks or actions that run before/after a workflow step.
//! Adding a new hook type = add an enum variant + a match arm in `run_hook`.

use crate::template::render;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

/// A hook that runs before or after a step.
///
/// Deserialized from TOML using the `type` key as a discriminant:
/// `{ type = "file_non_empty", path = "..." }` or
/// `{ type = "script", command = "...", args = ["..."] }`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hook {
    /// Assert that the file at `path` exists and contains at least one byte.
    /// `path` may contain template placeholders (e.g. `"{{output_path}}"`).
    FileNonEmpty { path: String },

    /// Spawn an external process.
    ///
    /// `command` is the executable name or absolute path.
    /// `args` are the arguments; each element may contain template placeholders.
    Script { command: String, args: Vec<String> },

    /// Push any unpushed commits in the workspace to the remote.
    ///
    /// If there are unpushed commits, pushes them. If there are no new commits
    /// (HEAD matches the upstream), the hook fails — code must be committed and
    /// pushed for this hook to pass.
    PushCode,
}

/// Execute a single hook, resolving any template placeholders in its arguments.
///
/// On failure writes a human-readable message to `error_path` and returns Err.
///
/// `token` and `current_exe` are required by `PushCode` to authenticate git
/// push operations via the ASKPASS mechanism. Other hook types ignore them.
pub fn run_hook(
    hook: &Hook,
    vars: &HashMap<&str, String>,
    error_path: &Path,
    token: &str,
    current_exe: &Path,
) -> Result<()> {
    match hook {
        Hook::FileNonEmpty { path: raw_path } => {
            let path = render(raw_path, vars);
            match fs::metadata(&path) {
                Ok(m) if m.len() > 0 => Ok(()),
                Ok(_) => {
                    let msg = format!("hook FileNonEmpty: file is empty: {}", path);
                    let _ = fs::write(error_path, &msg);
                    anyhow::bail!("{}", msg);
                }
                Err(e) => {
                    let msg = format!("hook FileNonEmpty: file missing ({}): {}", path, e);
                    let _ = fs::write(error_path, &msg);
                    anyhow::bail!("{}", msg);
                }
            }
        }

        Hook::Script { command, args } => {
            let resolved_args: Vec<String> = args.iter().map(|a| render(a, vars)).collect();

            tracing::info!(command, args = resolved_args.join(" "), "running hook");

            let mut child = Command::new(command)
                .args(&resolved_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    anyhow::anyhow!("hook Script: failed to spawn `{}`: {}", command, e)
                })?;

            // Stream stderr on a dedicated thread (avoids pipe deadlock).
            let stderr_stream = child.stderr.take();
            let stderr_thread = std::thread::spawn(move || {
                if let Some(stderr) = stderr_stream {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        tracing::error!(stderr = true, "{}", line);
                    }
                }
            });

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    tracing::info!("{}", line);
                }
            }

            let _ = stderr_thread.join();
            let status = child.wait()?;

            if status.success() {
                Ok(())
            } else {
                let code = status.code().unwrap_or(-1);
                let msg = format!("hook Script: `{}` exited with code {}", command, code);
                let _ = fs::write(error_path, &msg);
                anyhow::bail!("{}", msg);
            }
        }

        Hook::PushCode => {
            let workspace = vars
                .get("workspace")
                .ok_or_else(|| anyhow::anyhow!("hook PushCode: workspace variable missing"))?;
            let workspace_path = Path::new(workspace);

            // Check for unpushed commits: git rev-list @{u}..HEAD
            // This lists commits reachable from HEAD but not from the upstream branch.
            let check_cmd = crate::git::git_command(token, current_exe)
                .args(["rev-list", "@{u}..HEAD"])
                .current_dir(workspace_path)
                .output()?;

            if check_cmd.status.success() && !check_cmd.stdout.is_empty() {
                // Unpushed commits exist — push them.
                tracing::info!("hook PushCode: unpushed commits detected, pushing to remote");
                let push_cmd = crate::git::git_command(token, current_exe)
                    .args(["push"])
                    .current_dir(workspace_path)
                    .output()?;

                if !push_cmd.status.success() {
                    let stderr = String::from_utf8_lossy(&push_cmd.stderr);
                    let msg = format!("hook PushCode: git push failed: {}", stderr);
                    let _ = fs::write(error_path, &msg);
                    anyhow::bail!("{}", msg);
                }
                tracing::info!("hook PushCode: push succeeded");
                Ok(())
            } else {
                // No unpushed commits. Either HEAD matches the upstream
                // (nothing new to push) or git rev-list failed (no upstream set).
                // Either way, the hook requirement is that code must have been
                // committed and pushed — if there's nothing to push, the step
                // didn't produce any changes.
                let msg = "hook PushCode: no new commits found to push. \
                    Code must be committed and pushed to pass this hook.";
                let _ = fs::write(error_path, msg);
                anyhow::bail!("{}", msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_non_empty_hook_deserializes() {
        let toml = r#"
type = "file_non_empty"
path = "{{output_path}}"
"#;
        let hook: Hook = toml::from_str(toml).unwrap();
        assert!(matches!(hook, Hook::FileNonEmpty { .. }));
    }

    #[test]
    fn script_hook_deserializes() {
        let toml = r#"
type = "script"
command = "cargo"
args = ["clippy"]
"#;
        let hook: Hook = toml::from_str(toml).unwrap();
        match hook {
            Hook::Script { command, args } => {
                assert_eq!(command, "cargo");
                assert_eq!(args, vec!["clippy"]);
            }
            other => panic!("unexpected hook: {:?}", other),
        }
    }

    #[test]
    fn push_code_hook_deserializes() {
        let toml = r#"
type = "push_code"
"#;
        let hook: Hook = toml::from_str(toml).unwrap();
        assert!(matches!(hook, Hook::PushCode));
    }

    #[test]
    fn file_non_empty_succeeds_for_nonempty_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"content").unwrap();
        let path = f.path().to_string_lossy().into_owned();

        let hook = Hook::FileNonEmpty { path };
        let mut vars = HashMap::new();
        vars.insert("output_path", "/tmp".to_string());
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path(), "fake-token", Path::new("/fake/exe")).is_ok());
    }

    #[test]
    fn file_non_empty_fails_for_missing_file() {
        let hook = Hook::FileNonEmpty {
            path: "/nonexistent/file.xyz".to_string(),
        };
        let vars = HashMap::new();
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path(), "fake-token", Path::new("/fake/exe")).is_err());
    }

    #[test]
    fn file_non_empty_fails_for_empty_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_string_lossy().into_owned();

        let hook = Hook::FileNonEmpty { path };
        let vars = HashMap::new();
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path(), "fake-token", Path::new("/fake/exe")).is_err());
    }
}