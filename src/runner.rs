use crate::git;
use crate::hermes::{InvokeArgs, invoke};
use crate::template::render;
use crate::workflow::{Hook, Step};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
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

/// Execute a single hook, resolving any template placeholders in its arguments.
///
/// On failure writes a human-readable message to `error_path` and returns Err.
fn run_hook(hook: &Hook, vars: &HashMap<&str, String>, error_path: &Path) -> Result<()> {
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

        // Create a span for the entire step iteration — pre-hooks, hermes,
        // post-hooks, and all their log output inherit this context.
        let span = info_span!(
            "step",
            profile = %step.profile,
            issue = %key,
            step_name = %step.name,
        );
        let _enter = span.enter();

        // --- Pre-hooks -----------------------------------------------------------
        for hook in &step.pre_hooks {
            run_hook(hook, &vars, &error_path).map_err(|e| {
                tracing::error!(step = step.name, "pre-hook FAILED: {}", e);
                e
            })?;
        }

        tracing::info!("started");

        // hermes::invoke is sync; run it in a blocking thread pool.
        let hermes_args = InvokeArgs {
            prompt: render(&step.prompt_template, &vars),
            profile: step.profile.clone(),
            worktree: step.worktree,
            provider: step.provider.clone(),
            model: step.model.clone(),
            error_file: error_path.clone(),
            work_dir: Some(workspace_dir.clone()),
        };

        // Clone the span so that spawn_blocking carries it into the new thread.
        let hermes_span = span.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _enter = hermes_span.enter();
            invoke(&hermes_args)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {}", e))?;
        result?;

        // --- Post-hooks ----------------------------------------------------------
        for hook in &step.post_hooks {
            run_hook(hook, &vars, &error_path).map_err(|e| {
                tracing::error!(step = step.name, "post-hook FAILED: {}", e);
                e
            })?;
        }

        tracing::info!("completed");
    }

    Ok(())
}
