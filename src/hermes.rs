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
    let parent_span = Span::current();
    let stderr_issue = args.issue.clone();
    let stderr_step = args.step.clone();

    let log_file = std::fs::File::create(&args.log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", args.log_file, e))?;
    let log_writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>> =
        Arc::new(Mutex::new(std::io::BufWriter::new(log_file)));
    let log_writer_stderr = Arc::clone(&log_writer);
    let log_writer_stdout = Arc::clone(&log_writer);
    let show_logs = args.show_logs;

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

    let _ = stderr_thread.join();
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
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]);
                if let Some(capture) = stderr_capture {
                    let mut cap = capture.lock().unwrap_or_else(|e| e.into_inner());
                    cap.push_str(&chunk);
                }
                let current_text = remainder + &chunk;
                if let Some(last_nl) = current_text.rfind('\n') {
                    let lines_text = &current_text[..=last_nl];
                    remainder = current_text[last_nl + 1..].to_string();
                    {
                        let mut writer = log_writer.lock().unwrap_or_else(|e| e.into_inner());
                        for line in lines_text.lines() {
                            let _ = writeln!(writer, "{}", timestamp_line(line));
                        }
                        let _ = writer.flush();
                    }
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
pub struct HermesApiHarness {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
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

struct InvokeApiArgs<'a> {
    base_url: &'a str,
    api_key: &'a str,
    model: &'a str,
    prompt: &'a str,
    error_file: &'a Path,
    log_file: &'a PathBuf,
    show_logs: bool,
    issue: &'a str,
}

fn invoke_api(args: &InvokeApiArgs<'_>) -> Result<()> {
    use reqwest::blocking::Client;
    use serde_json::json;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {}", e))?;

    let base_url = args.base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    let body = json!({
        "model": args.model,
        "messages": [
            {"role": "user", "content": args.prompt}
        ],
        "stream": false
    });

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", args.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

    let status = response.status();
    let response_text = response
        .text()
        .map_err(|e| anyhow::anyhow!("failed to read response: {}", e))?;

    let file = std::fs::File::create(args.log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", args.log_file, e))?;
    let mut writer = std::io::BufWriter::new(file);

    let timestamped_response = timestamp_line(&format!(
        "API Response (status: {}):\n{}",
        status, response_text
    ));
    writeln!(writer, "{}", timestamped_response)
        .map_err(|e| anyhow::anyhow!("failed to write to log file: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if args.show_logs {
        tracing::info!(issue = %args.issue, "API response status: {}", status);
        tracing::info!(issue = %args.issue, "API response: {}", response_text);
    }

    if !status.is_success() {
        let error_content = format!("HTTP Error: {}\n\nResponse:\n{}", status, response_text);
        if let Err(e) = std::fs::write(args.error_file, &error_content) {
            tracing::warn!("failed to write error file {:?}: {}", args.error_file, e);
        }
        anyhow::bail!("API request failed with status {}", status);
    }

    let response_json: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| anyhow::anyhow!("failed to parse response JSON: {}", e))?;

    let assistant_message = response_json
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("<no content in response>");

    let message_log = timestamp_line(&format!("Assistant: {}", assistant_message));
    writeln!(writer, "{}", message_log)
        .map_err(|e| anyhow::anyhow!("failed to write message to log: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if args.show_logs {
        tracing::info!(issue = %args.issue, "Assistant response: {}", assistant_message);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    #[test]
    fn test_timestamp_line_format() {
        let result = timestamp_line("hello world");
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
        assert!(
            log_contents.contains("LINE 001"),
            "log is missing the beginning of output"
        );
        assert!(
            log_contents.contains("LINE 1000"),
            "log is missing the end of output"
        );
    }

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
    }
}
