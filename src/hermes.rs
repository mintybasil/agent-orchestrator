use crate::harness::Harness;
use crate::workflow::Step;
use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tracing::Span;

/// Arguments for invoking the hermes CLI agent.
///
/// Grouped into a struct to keep the [`invoke`] signature manageable and
/// make call-sites self-documenting.
pub struct InvokeArgs {
    pub prompt: String,
    pub profile: String,
    pub worktree: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub error_file: PathBuf,
    pub work_dir: Option<PathBuf>,
    pub issue: String,
    pub step: String,
}

/// Invoke `hermes chat` with the given arguments.
///
/// Always passes `--yolo` and `--quiet`. If `work_dir` is provided, hermes
/// runs from that directory (which becomes its project root).
///
/// Log lines include explicit `issue` and `step` fields from `InvokeArgs`,
/// ensuring context is visible regardless of tracing formatter configuration.
/// The current span is also propagated to the stderr drain thread via
/// `Span::current()`.
///
/// Returns `Ok(())` on exit code 0.
/// On non-zero exit: writes captured stderr to `error_file` and returns `Err`.
pub fn invoke(args: &InvokeArgs) -> Result<()> {
    let mut cmd = Command::new("hermes");
    if let Some(dir) = &args.work_dir {
        cmd.current_dir(dir);
    }
    cmd.arg("chat")
        .arg("-p")
        .arg(&args.prompt)
        .arg("--yolo")
        .arg("--quiet")
        .arg("--profile")
        .arg(&args.profile);
    if args.worktree {
        cmd.arg("--worktree");
    }
    if let Some(provider) = &args.provider {
        cmd.arg("--provider").arg(provider);
    }
    if let Some(model) = &args.model {
        cmd.arg("--model").arg(model);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn hermes: {}", e))?;

    let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // Propagate the current tracing span to the stderr drain thread so that
    // concurrent stderr output carries the same span context as stdout.
    let parent_span = Span::current();

    // Clone context strings for the stderr drain thread (which is a `move` closure).
    let stderr_issue = args.issue.clone();
    let stderr_step = args.step.clone();

    // Drain stderr on a dedicated thread (concurrent with stdout drain)
    let stderr_stream = child.stderr.take();
    let stderr_capture_clone = Arc::clone(&stderr_capture);
    let stderr_thread = std::thread::spawn(move || {
        let _enter = parent_span.enter();
        if let Some(stderr) = stderr_stream {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        tracing::error!(issue = %stderr_issue, step = %stderr_step, "{}", l);
                        let mut cap = stderr_capture_clone
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        cap.push_str(&l);
                        cap.push('\n');
                    }
                    Err(e) => {
                        tracing::warn!(issue = %stderr_issue, step = %stderr_step, "stderr read error: {}", e)
                    }
                }
            }
        }
    });

    // Drain stdout in this thread (already inside the caller's span)
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => tracing::info!(issue = %args.issue, step = %args.step, "{}", l),
                Err(e) => {
                    tracing::warn!(issue = %args.issue, step = %args.step, "stdout read error: {}", e)
                }
            }
        }
    }

    // Wait for stderr thread to finish
    let _ = stderr_thread.join();

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        let stderr_text = stderr_capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Err(e) = std::fs::write(&args.error_file, &stderr_text) {
            tracing::warn!("failed to write error file {:?}: {}", args.error_file, e);
        }
        anyhow::bail!("hermes exited with code {}", code);
    }
}

/// Harness implementation for the hermes CLI agent.
///
/// Carries hermes-specific options (profile, worktree, provider, model)
/// that were specified in the harness config, not on the generic Step.
pub struct HermesHarness {
    pub profile: String,
    pub worktree: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Harness for HermesHarness {
    fn name(&self) -> &str {
        "hermes"
    }

    fn run_step(
        &self,
        step: &Step,
        workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let args = InvokeArgs {
            prompt: rendered_prompt.to_string(),
            profile: self.profile.clone(),
            worktree: self.worktree,
            provider: self.provider.clone(),
            model: self.model.clone(),
            error_file: error_path.to_path_buf(),
            work_dir: Some(workspace_dir.to_path_buf()),
            issue: issue.to_string(),
            step: step.name.clone(),
        };

        Box::pin(async move { tokio::task::spawn_blocking(move || invoke(&args)).await? })
    }
}
