use crate::hermes::invoke;
use crate::template::render;
use crate::workflow::{Hook, Step};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

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
        Hook::FileNotEmpty { path: raw_path } => {
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

            tracing::info!("[hook Script] {} {}", command, resolved_args.join(" "));

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
                        tracing::error!("[hook stderr] {}", line);
                    }
                }
            });

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    tracing::info!("[hook stdout] {}", line);
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
pub async fn run_issue(key: &IssueKey, data_root: &Path, steps: &[Step]) -> Result<()> {
    let issue_dir = data_root
        .join(&key.owner)
        .join(&key.repo)
        .join(key.number.to_string());
    fs::create_dir_all(&issue_dir)?;

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
        ];
        let vars: HashMap<&str, String> = var_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

        // --- Pre-hooks -----------------------------------------------------------
        for hook in &step.pre_hooks {
            run_hook(hook, &vars, &error_path).map_err(|e| {
                tracing::error!("[{}] {}: pre-hook FAILED: {}", key, step.name, e);
                e
            })?;
        }

        tracing::info!("[{}] {}: started", key, step.name);

        // hermes::invoke is sync; run it in a blocking thread pool.
        let prompt = render(&step.prompt_template, &vars);
        let profile = step.profile.clone();
        let worktree = step.worktree;
        let provider = step.provider.clone();
        let model = step.model.clone();
        let error_path_clone = error_path.clone();
        tokio::task::spawn_blocking(move || {
            invoke(
                &prompt,
                &profile,
                worktree,
                provider.as_deref(),
                model.as_deref(),
                &error_path_clone,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {}", e))??;

        // --- Post-hooks ----------------------------------------------------------
        for hook in &step.post_hooks {
            run_hook(hook, &vars, &error_path).map_err(|e| {
                tracing::error!("[{}] {}: post-hook FAILED: {}", key, step.name, e);
                e
            })?;
        }

        tracing::info!("[{}] {}: completed", key, step.name);
    }

    Ok(())
}
