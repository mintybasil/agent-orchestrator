use crate::harness::Harness;
use crate::workflow::Step;
use anyhow::Result;
use chrono::Utc;
use std::io::{Read, Write};
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

    // Create the log file with a BufWriter for efficient, flushed writes.
    // Both stdout and stderr threads write here.
    let log_file = std::fs::File::create(&args.log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", args.log_file, e))?;
    let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
        Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
    let log_writer_stderr = Arc::clone(&log_writer);
    let log_writer_stdout = Arc::clone(&log_writer);

    let show_logs = args.show_logs;

    // Drain stderr on a dedicated thread (concurrent with stdout drain)
    let stderr_stream = child.stderr.take();
    let stderr_capture_clone = Arc::clone(&stderr_capture);
    let stderr_thread = std::thread::spawn(move || {
        let _enter = parent_span.enter();
        if let Some(stderr) = stderr_stream {
            drain_stream(
                stderr,
                &log_writer_stderr,
                Some(&stderr_capture_clone),
                show_logs,
                true,
                &stderr_issue,
                &stderr_step,
            );
        }
    });

    // Drain stdout in this thread (already inside the caller's span)
    if let Some(stdout) = child.stdout.take() {
        drain_stream(
            stdout,
            &log_writer_stdout,
            None,
            show_logs,
            false,
            &args.issue,
            &args.step,
        );
    }

    // Wait for stderr thread to finish
    let _ = stderr_thread.join();

    // Final flush of any remaining buffered log data
    {
        let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.flush();
    }

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

/// Read all data from a child process stream into a log file.
///
/// Uses a byte-buffer `read()` loop instead of `BufReader::lines()` to ensure
/// no data is lost due to line-buffering or incomplete final lines. Data is
/// written to the log file chunk-by-chunk as it arrives and flushed after each
/// write to guarantee completeness even if the process exits abruptly.
///
/// Each line written to the log file is prefixed with a UTC timestamp via
/// [`timestamp_line`]. The raw (un-timestamped) text is captured in
/// `stderr_capture` for error reporting, and traced without timestamps when
/// `show_logs` is true.
///
/// When `show_logs` is true, each complete line within a chunk is also emitted
/// via tracing. Partial lines at chunk boundaries are not traced immediately
/// (they are written to the log file immediately though) and will appear in a
/// subsequent tracing call when the newline arrives.
fn drain_stream(
    mut stream: impl Read,
    log_writer: &Arc<Mutex<std::io::BufWriter<std::fs::File>>>,
    stderr_capture: Option<&Arc<Mutex<String>>>,
    show_logs: bool,
    is_stderr: bool,
    issue: &str,
    step: &str,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]);

                // Write timestamped lines to log file and flush to ensure data hits disk promptly
                {
                    let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
                    for line in chunk.lines() {
                        let _ = writeln!(writer, "{}", timestamp_line(line));
                    }
                    let _ = writer.flush();
                }

                // Capture raw (un-timestamped) stderr for error_file on failure
                if let Some(capture) = stderr_capture {
                    let mut cap = capture.lock().unwrap_or_else(|e| e.into_inner());
                    cap.push_str(&chunk);
                }

                // Trace complete lines if show_logs is enabled
                if show_logs {
                    for line in chunk.lines() {
                        if is_stderr {
                            tracing::error!(issue = %issue, step = %step, "{}", line);
                        } else {
                            tracing::info!(issue = %issue, step = %step, "{}", line);
                        }
                    }
                }
            }
            Err(e) => {
                let stream_name = if is_stderr { "stderr" } else { "stdout" };
                tracing::warn!(
                    issue = %issue,
                    step = %step,
                    "{} read error: {}",
                    stream_name,
                    e
                );
                break;
            }
        }
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
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    // --- timestamp_line tests ---

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
        let re = Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\] $").unwrap();
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

    // --- Log capture regression tests ---

    /// Regression test: verify that the log captures the beginning of output,
    /// not just the end. Spawns a child that prints 1000 numbered lines and
    /// checks that "LINE 001" appears in the log file.
    #[test]
    fn test_log_capture_completeness() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("for i in $(seq 1 1000); do printf 'LINE %03d\\n' \"$i\"; done")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn test process");

        let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let log_file = std::fs::File::create(&log_path).expect("failed to create log file");
        let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
            Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
        let log_writer_clone = Arc::clone(&log_writer);
        let stderr_capture_clone = Arc::clone(&stderr_capture);

        let stderr_stream = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr_stream {
                drain_stream(
                    stderr,
                    &log_writer_clone,
                    Some(&stderr_capture_clone),
                    false,
                    true,
                    "test/issue",
                    "test-step",
                );
            }
        });

        if let Some(stdout) = child.stdout.take() {
            drain_stream(
                stdout,
                &log_writer,
                None,
                false,
                false,
                "test/issue",
                "test-step",
            );
        }

        let _ = stderr_thread.join();
        {
            let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writer.flush();
        }

        let status = child.wait().expect("failed to wait for child");
        assert!(status.success());

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        // Each line should be timestamped, so check for the content after the timestamp
        assert!(
            log_contents.contains("LINE 001"),
            "log is missing the beginning of output. First 200 chars: {}",
            &log_contents[..log_contents.len().min(200)]
        );
        assert!(
            log_contents.contains("LINE 1000"),
            "log is missing the end of output"
        );
        // Verify timestamp prefix is present on lines
        let re = Regex::new(r"\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\]").unwrap();
        assert!(
            re.is_match(&log_contents),
            "log lines should have timestamp prefix"
        );
    }

    /// Test that output without a trailing newline is fully captured.
    /// The old `lines()` implementation would silently drop such data.
    #[test]
    fn test_log_capture_no_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf 'first line\\nsecond line\\nno trailing newline'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn test process");

        let log_file = std::fs::File::create(&log_path).expect("failed to create log file");
        let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
            Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
        let log_writer_clone = Arc::clone(&log_writer);

        let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let stderr_capture_clone = Arc::clone(&stderr_capture);
        let stderr_stream = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr_stream {
                drain_stream(
                    stderr,
                    &log_writer_clone,
                    Some(&stderr_capture_clone),
                    false,
                    true,
                    "test/issue",
                    "test-step",
                );
            }
        });

        if let Some(stdout) = child.stdout.take() {
            drain_stream(
                stdout,
                &log_writer,
                None,
                false,
                false,
                "test/issue",
                "test-step",
            );
        }

        let _ = stderr_thread.join();
        {
            let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writer.flush();
        }

        let status = child.wait().expect("failed to wait for child");
        assert!(status.success());

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log_contents.contains("first line"),
            "log is missing 'first line'"
        );
        assert!(
            log_contents.contains("second line"),
            "log is missing 'second line'"
        );
        assert!(
            log_contents.contains("no trailing newline"),
            "log is missing partial last line 'no trailing newline'"
        );
    }

    /// Test that both stdout and stderr are captured in the log file.
    #[test]
    fn test_log_capture_stdout_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("echo 'stdout message'; echo 'stderr message' >&2")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn test process");

        let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let log_file = std::fs::File::create(&log_path).expect("failed to create log file");
        let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
            Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
        let log_writer_stderr = Arc::clone(&log_writer);
        let log_writer_stdout = Arc::clone(&log_writer);

        let stderr_stream = child.stderr.take();
        let stderr_capture_clone = Arc::clone(&stderr_capture);
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr_stream {
                drain_stream(
                    stderr,
                    &log_writer_stderr,
                    Some(&stderr_capture_clone),
                    false,
                    true,
                    "test/issue",
                    "test-step",
                );
            }
        });

        if let Some(stdout) = child.stdout.take() {
            drain_stream(
                stdout,
                &log_writer_stdout,
                None,
                false,
                false,
                "test/issue",
                "test-step",
            );
        }

        let _ = stderr_thread.join();
        {
            let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writer.flush();
        }

        let status = child.wait().expect("failed to wait for child");
        assert!(status.success());

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log_contents.contains("stdout message"),
            "log is missing stdout output"
        );
        assert!(
            log_contents.contains("stderr message"),
            "log is missing stderr output"
        );

        // stderr should also be captured separately for the error file
        let stderr_text = stderr_capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            stderr_text.contains("stderr message"),
            "stderr capture is missing stderr output"
        );
    }
}