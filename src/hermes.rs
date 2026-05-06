use crate::harness::Harness;
use crate::workflow::Step;
use anyhow::Result;
use chrono::Utc;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tracing::Span;

/// Prepend a UTC timestamp to a line for log file output.
///
/// Format: `[YYYY-MM-DD HH:MM:SS UTC] <line>`
fn timestamp_line(line: &str) -> String {
    format!("[{}] {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), line)
}

/// Arguments for invoking the hermes CLI agent.
///
/// Grouped into a struct to keep the [`invoke`] signature manageable and
/// make call-sites self-documenting.
pub struct InvokeArgs {
    pub prompt: String,
    pub profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub error_file: PathBuf,
    pub log_file: PathBuf,
    pub show_logs: bool,
    pub work_dir: Option<PathBuf>,
    pub issue: String,
    pub step: String,
}

/// Invoke `hermes chat` with the given arguments.
///
/// Always passes `--yolo` and `--quiet`. If `work_dir` is provided, hermes
/// runs from that directory (which becomes its project root).
///
/// All stdout and stderr output is written to `args.log_file`. When
/// `args.show_logs` is true, output is also printed via tracing.
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
        .arg("-q")
        .arg(&args.prompt)
        .arg("--yolo")
        .arg("--quiet")
        .arg("--profile")
        .arg(&args.profile);
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

    // Create the log file. Both stdout and stderr threads write here.
    let log_file = std::fs::File::create(&args.log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", args.log_file, e))?;
    let log_file_stdout = Arc::new(Mutex::new(log_file));
    let log_file_stderr = Arc::clone(&log_file_stdout);

    let show_logs = args.show_logs;

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
                        if show_logs {
                            tracing::error!(issue = %stderr_issue, step = %stderr_step, "{}", l);
                        }
                        // Write to log file
                        {
                            let mut file =
                                log_file_stderr.lock().unwrap_or_else(|e| e.into_inner());
                            use std::io::Write;
                            let _ = writeln!(file, "{}", timestamp_line(&l));
                        }
                        // Capture for error_file on failure
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
                Ok(l) => {
                    if show_logs {
                        tracing::info!(issue = %args.issue, step = %args.step, "{}", l);
                    }
                    // Write to log file
                    {
                        let mut file = log_file_stdout.lock().unwrap_or_else(|e| e.into_inner());
                        use std::io::Write;
                        let _ = writeln!(file, "{}", timestamp_line(&l));
                    }
                }
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
/// Carries hermes-specific options (profile, provider, model)
/// that were specified in the harness config, not on the generic Step.
pub struct HermesHarness {
    pub profile: String,
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
        log_config: &crate::harness::LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let args = InvokeArgs {
            prompt: rendered_prompt.to_string(),
            profile: self.profile.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            error_file: error_path.to_path_buf(),
            log_file: log_config.log_path.clone(),
            show_logs: log_config.show_logs,
            work_dir: Some(workspace_dir.to_path_buf()),
            issue: issue.to_string(),
            step: step.name.clone(),
        };

        Box::pin(async move { tokio::task::spawn_blocking(move || invoke(&args)).await? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn test_timestamp_line_format() {
        let result = timestamp_line("hello world");
        // Should match [YYYY-MM-DD HH:MM:SS UTC] hello world
        let re = Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\] hello world$").unwrap();
        assert!(
            re.is_match(&result),
            "timestamp_line output did not match expected format: {result}"
        );
    }

    #[test]
    fn test_timestamp_line_empty() {
        let result = timestamp_line("");
        let re =
            Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\] $").unwrap();
        assert!(
            re.is_match(&result),
            "timestamp_line with empty input did not match: {result}"
        );
    }

    #[test]
    fn test_timestamp_line_preserves_content() {
        let content = "some log output with special chars: !@#$%^&*()";
        let result = timestamp_line(content);
        assert!(
            result.ends_with(content),
            "timestamp_line should preserve the original content at the end: {result}"
        );
    }
}
