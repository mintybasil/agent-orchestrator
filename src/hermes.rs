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
    pub max_turns: Option<u32>,
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
    if let Some(max_turns) = args.max_turns {
        cmd.arg("--max-turns").arg(max_turns.to_string());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn hermes: {}", e))?;

    let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // Propagate the current tracing span to the stderr drain thread so that
    // concurrent stderr output carries the same span context.
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
    let mut remainder = String::new();
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]);

                // Capture raw (un-timestamped) stderr for error_file on failure.
                // Always push the raw chunk, not the reconstructed text, so that
                // the capture is byte-identical to what the process wrote.
                if let Some(capture) = stderr_capture {
                    let mut cap = capture.lock().unwrap_or_else(|e| e.into_inner());
                    cap.push_str(&chunk);
                }

                // Prepend any leftover from the previous chunk and split at the
                // last newline boundary. Everything up to (and including) the
                // last newline forms complete lines ready for timestamping. The
                // tail after the last newline is kept in `remainder` for the
                // next iteration.
                let current_text = remainder + &chunk;
                if let Some(last_nl) = current_text.rfind('\n') {
                    let lines_text = &current_text[..=last_nl];
                    remainder = current_text[last_nl + 1..].to_string();

                    // Write timestamped lines to log file and flush promptly
                    {
                        let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
                        for line in lines_text.lines() {
                            let _ = writeln!(writer, "{}", timestamp_line(line));
                        }
                        let _ = writer.flush();
                    }

                    // Trace complete lines if show_logs is enabled
                    if show_logs {
                        for line in lines_text.lines() {
                            if is_stderr {
                                tracing::error!(issue = %issue, step = %step, "{}", line);
                            } else {
                                tracing::info!(issue = %issue, step = %step, "{}", line);
                            }
                        }
                    }
                } else {
                    // No newline in this chunk — keep accumulating in remainder
                    remainder = current_text;
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

    // Flush any remaining partial line after EOF
    if !remainder.is_empty() {
        {
            let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(writer, "{}", timestamp_line(&remainder));
            let _ = writer.flush();
        }
        if show_logs {
            if is_stderr {
                tracing::error!(issue = %issue, step = %step, "{}", remainder);
            } else {
                tracing::info!(issue = %issue, step = %step, "{}", remainder);
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
    pub max_turns: Option<u32>,
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
            max_turns: self.max_turns,
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

/// Harness implementation for the hermes API server.
///
/// Uses the OpenAI-compatible `/v1/chat/completions` endpoint to invoke hermes.
/// The API server captures all agent output (including tool calls) in the HTTP response.
pub struct HermesApiHarness {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: Option<u32>,
}

impl Harness for HermesApiHarness {
    fn name(&self) -> &str {
        "hermes-api"
    }

    fn run_step(
        &self,
        _step: &Step,
        _workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
        log_config: &crate::harness::LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let prompt = rendered_prompt.to_string();
        let error_file = error_path.to_path_buf();
        let log_file = log_config.log_path.clone();
        let show_logs = log_config.show_logs;
        let issue_str = issue.to_string();

        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                invoke_api(
                    &base_url,
                    &api_key,
                    &model,
                    &prompt,
                    &error_file,
                    &log_file,
                    show_logs,
                    &issue_str,
                )
            })
            .await?
        })
    }
}

/// Invoke hermes via the HTTP API server.
///
/// Sends a POST request to `{base_url}/chat/completions` with the prompt
/// and captures the response. All output is written to `log_file`.
///
/// Returns `Ok(())` on successful completion.
/// On error: writes error details to `error_file` and returns `Err`.
fn invoke_api(
    base_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    error_file: &Path,
    log_file: &PathBuf,
    show_logs: bool,
    issue: &str,
) -> Result<()> {
    use reqwest::blocking::Client;
    use serde_json::json;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {}", e))?;

    // Ensure base_url doesn't have trailing slash for consistent URL construction
    let base_url = base_url.trim_end_matches('/');
    
    // Build the API endpoint URL
    // The API server supports both /v1/chat/completions and /chat/completions
    // We try /chat/completions first (more common pattern)
    let url = format!("{}/chat/completions", base_url);

    // Build the request body matching OpenAI chat completions format
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "stream": false
    });

    // Add max_turns if specified (hermes-specific parameter)
    // Note: The API server may not support this directly, but we include it for future compatibility
    // For now, we rely on server-side configuration for turn limits

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

    let status = response.status();
    let response_text = response
        .text()
        .map_err(|e| anyhow::anyhow!("failed to read response: {}", e))?;

    // Create log file and writer
    let file = std::fs::File::create(log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", log_file, e))?;
    let mut writer = std::io::BufWriter::new(file);

    // Write response to log file with timestamp
    let timestamped_response = timestamp_line(&format!(
        "API Response (status: {}):\n{}",
        status, response_text
    ));
    writeln!(writer, "{}", timestamped_response)
        .map_err(|e| anyhow::anyhow!("failed to write to log file: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if show_logs {
        tracing::info!(issue = %issue, "API response status: {}", status);
        tracing::info!(issue = %issue, "API response: {}", response_text);
    }

    // Check if the request was successful
    if !status.is_success() {
        // Write error details to error file
        let error_content = format!(
            "HTTP Error: {}\n\nResponse:\n{}",
            status, response_text
        );
        if let Err(e) = std::fs::write(error_file, &error_content) {
            tracing::warn!("failed to write error file {:?}: {}", error_file, e);
        }
        anyhow::bail!("API request failed with status {}", status);
    }

    // Parse the response to extract the assistant's message
    let response_json: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| anyhow::anyhow!("failed to parse response JSON: {}", e))?;

    // Extract the assistant's response content
    let assistant_message = response_json
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("<no content in response>");

    // Log the extracted message
    let message_log = timestamp_line(&format!("Assistant: {}", assistant_message));
    writeln!(writer, "{}", message_log)
        .map_err(|e| anyhow::anyhow!("failed to write message to log: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if show_logs {
        tracing::info!(issue = %issue, "Assistant response: {}", assistant_message);
    }

    Ok(())
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

    /// Regression test: verify that output spanning chunk boundaries is
    /// captured completely, without data loss. The old `chunk.lines()`
    /// implementation would only emit a line when a `\n` was present in
    /// the current chunk; a long string with no newlines was silently
    /// dropped if it exceeded the 8 KiB read buffer.
    #[test]
    fn test_log_capture_chunk_boundary_truncation() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("boundary.log");

        // Print 10KB of data with no newlines — must span multiple 8KB read chunks
        let long_string = "A".repeat(10_000);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '{}'", long_string))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");

        let log_file = std::fs::File::create(&log_path).expect("failed to create log file");
        let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
            Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
        let log_writer_clone = Arc::clone(&log_writer);

        let stderr_stream = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr_stream {
                drain_stream(stderr, &log_writer_clone, None, false, true, "test", "step");
            }
        });

        if let Some(stdout) = child.stdout.take() {
            drain_stream(stdout, &log_writer, None, false, false, "test", "step");
        }

        let _ = stderr_thread.join();
        {
            let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writer.flush();
        }

        let _ = child.wait().expect("failed to wait for child");

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log_contents.contains(&long_string),
            "Log missing data across chunk boundaries. Log length: {}, expected substring length: {}",
            log_contents.len(),
            long_string.len()
        );
    }
}
