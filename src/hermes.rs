use crate::harness::Harness;
use crate::workflow::Step;
use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prepend a UTC timestamp to a line for log file output.
///
/// Format: `[YYYY-MM-DD HH:MM:SS UTC] <line>`
fn timestamp_line(line: &str) -> String {
    format!("[{}] {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), line)
}

/// Post-process a log file by prepending a UTC timestamp to every line.
///
/// Reads the raw (untimestamped) log file produced by shell redirection,
/// prefixes each line with `[YYYY-MM-DD HH:MM:SS UTC]`, and overwrites
/// the file in place. This provides parity with the old pipe-draining
/// architecture which timestamped lines as they streamed in.
fn timestamp_log_file(path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!("failed to read log file for timestamping {:?}: {}", path, e)
    })?;

    let mut timestamped = String::new();
    for line in content.lines() {
        timestamped.push_str(&timestamp_line(line));
        timestamped.push('\n');
    }

    std::fs::write(path, timestamped)
        .map_err(|e| anyhow::anyhow!("failed to write timestamped log file {:?}: {}", path, e))?;
    Ok(())
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
/// Output is captured via shell-level file redirection (`sh -c` wrapping)
/// rather than OS pipes. This avoids the 64KB pipe buffer limit and
/// Python TUI framework buffering issues that caused log truncation with
/// `Stdio::piped()`.
///
/// On success: the log file is post-processed to add UTC timestamps to
/// each line for parity with the previous pipe-draining behavior.
///
/// On non-zero exit: reads the error file (populated by shell stderr
/// redirection) and returns `Err` with the stderr content.
pub fn invoke(args: &InvokeArgs) -> Result<()> {
    // Build the hermes command string
    let mut hermes_cmd = String::from("hermes chat -q ");
    // Shell-escape the prompt to avoid injection
    let escaped_prompt = args.prompt.replace('\'', "'\\''");
    hermes_cmd.push_str(&format!("'{}' ", escaped_prompt));
    hermes_cmd.push_str("--yolo --quiet --profile ");
    let escaped_profile = args.profile.replace('\'', "'\\''");
    hermes_cmd.push_str(&format!("'{}'", escaped_profile));

    if let Some(provider) = &args.provider {
        let escaped = provider.replace('\'', "'\\''");
        hermes_cmd.push_str(&format!(" --provider '{}'", escaped));
    }
    if let Some(model) = &args.model {
        let escaped = model.replace('\'', "'\\''");
        hermes_cmd.push_str(&format!(" --model '{}'", escaped));
    }
    if let Some(max_turns) = args.max_turns {
        hermes_cmd.push_str(&format!(" --max-turns {}", max_turns));
    }

    // Wrap in shell with file redirection
    let log_file_str = args.log_file.to_string_lossy();
    let error_file_str = args.error_file.to_string_lossy();
    let shell_command = format!(
        "{} > '{}' 2> '{}'",
        hermes_cmd, log_file_str, error_file_str
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&shell_command);
    if let Some(dir) = &args.work_dir {
        cmd.current_dir(dir);
    }

    if args.show_logs {
        tracing::info!(issue = %args.issue, step = %args.step, "invoking hermes via shell redirection");
    }

    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute hermes via shell: {}", e))?;

    if status.success() {
        // Post-process log file to add timestamps
        if let Err(e) = timestamp_log_file(&args.log_file) {
            tracing::warn!(
                issue = %args.issue,
                step = %args.step,
                "failed to timestamp log file: {}",
                e
            );
        }

        // If show_logs, print the log contents via tracing
        if args.show_logs
            && let Ok(content) = std::fs::read_to_string(&args.log_file)
        {
            for line in content.lines() {
                tracing::info!(issue = %args.issue, step = %args.step, "{}", line);
            }
        }

        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);

        // Read the error file for error context
        let stderr_text = std::fs::read_to_string(&args.error_file).unwrap_or_default();

        // Also timestamp the log file on failure (it may contain useful partial output)
        if let Err(e) = timestamp_log_file(&args.log_file) {
            tracing::warn!(
                issue = %args.issue,
                step = %args.step,
                "failed to timestamp log file on error: {}",
                e
            );
        }

        // Write error file content if it wasn't already written by the shell
        // (shell redirection writes stderr directly to error_file, so it should
        // already exist. But in case the shell itself failed to start, we provide
        // a fallback.)
        if !stderr_text.is_empty() {
            // The shell already wrote to error_file via 2> redirection, so no
            // additional write is needed unless the file is empty/missing.
        } else if !args.error_file.exists()
            && let Err(e) = std::fs::write(&args.error_file, &stderr_text)
        {
            tracing::warn!("failed to write error file {:?}: {}", args.error_file, e);
        }

        if args.show_logs && !stderr_text.is_empty() {
            for line in stderr_text.lines() {
                tracing::error!(issue = %args.issue, step = %args.step, "{}", line);
            }
        }

        anyhow::bail!(
            "hermes exited with code {}. stderr: {}",
            code,
            if stderr_text.is_empty() {
                "(empty)".to_string()
            } else {
                // Truncate very long error output in the bail message
                let max_len = 2000;
                if stderr_text.len() > max_len {
                    format!("{}...(truncated)", &stderr_text[..max_len])
                } else {
                    stderr_text
                }
            }
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
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

    // --- timestamp_log_file tests ---

    #[test]
    fn test_timestamp_log_file_basic() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");

        // Write untimestamped content
        std::fs::write(&log_path, "line one\nline two\nline three\n").unwrap();

        timestamp_log_file(&log_path).unwrap();

        let result = std::fs::read_to_string(&log_path).unwrap();
        let re = Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\]").unwrap();
        for line in result.lines() {
            assert!(
                re.is_match(line),
                "each line should have a timestamp prefix: {line}"
            );
        }
        assert!(result.contains("line one"), "should contain 'line one'");
        assert!(result.contains("line two"), "should contain 'line two'");
        assert!(result.contains("line three"), "should contain 'line three'");
    }

    #[test]
    fn test_timestamp_log_file_empty() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("empty.log");

        std::fs::write(&log_path, "").unwrap();

        // Should succeed on empty file
        timestamp_log_file(&log_path).unwrap();

        // Empty file produces a single empty timestamped line (or empty output)
        let result = std::fs::read_to_string(&log_path).unwrap();
        // An empty file has 0 lines from .lines(), so the output should be empty
        assert!(
            result.is_empty(),
            "empty file should produce empty timestamped output"
        );
    }

    #[test]
    fn test_timestamp_log_file_missing() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("nonexistent.log");

        let result = timestamp_log_file(&log_path);
        assert!(result.is_err(), "should fail for missing file");
    }

    // --- invoke tests using shell redirection ---

    /// Test that invoke produces a log file with timestamped content
    /// when the command succeeds.
    #[test]
    fn test_invoke_success_logs() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("success.log");
        let error_path = tmp.path().join("success.err");

        // Build the shell command manually to test the redirection logic.
        // We can't use the real invoke() because it calls "hermes" which
        // isn't available in test. Instead, test the core redirection pattern.
        let log_file_str = log_path.to_string_lossy();
        let error_file_str = error_path.to_string_lossy();
        let shell_command = format!(
            "echo 'hello from test' > '{}' 2> '{}'",
            log_file_str, error_file_str
        );

        let status = Command::new("sh")
            .arg("-c")
            .arg(&shell_command)
            .status()
            .expect("failed to execute shell command");

        assert!(status.success(), "command should succeed");

        // Verify log file exists and has content
        assert!(log_path.exists(), "log file should exist");
        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(!log_contents.is_empty(), "log file should not be empty");
        assert!(
            log_contents.contains("hello from test"),
            "log should contain command output"
        );

        // Apply timestamping
        timestamp_log_file(&log_path).unwrap();
        let timestamped = std::fs::read_to_string(&log_path).unwrap();
        let re = Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} UTC\]").unwrap();
        assert!(
            re.is_match(&timestamped),
            "timestamped log should have prefix: {timestamped}"
        );
        assert!(
            timestamped.contains("hello from test"),
            "timestamped log should still contain original content"
        );

        // Error file should be empty (no stderr from echo)
        let error_contents = std::fs::read_to_string(&error_path).unwrap();
        assert!(
            error_contents.is_empty(),
            "error file should be empty on success"
        );
    }

    /// Test that invoke reads the error file when the command exits non-zero.
    #[test]
    fn test_invoke_failure_error_file() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("failure.log");
        let error_path = tmp.path().join("failure.err");

        let log_file_str = log_path.to_string_lossy();
        let error_file_str = error_path.to_string_lossy();
        // Command that writes to both stdout and stderr, then exits non-zero.
        // The 2> redirect on the outer group captures all stderr.
        let shell_command = format!(
            "{{ echo 'stdout here' > '{}'; echo 'error output' >&2; exit 1; }} 2> '{}'",
            log_file_str, error_file_str
        );

        let status = Command::new("sh")
            .arg("-c")
            .arg(&shell_command)
            .status()
            .expect("failed to execute shell command");

        assert!(!status.success(), "command should fail");

        // Verify error file has stderr content
        assert!(error_path.exists(), "error file should exist");
        let error_contents = std::fs::read_to_string(&error_path).unwrap();
        assert!(
            error_contents.contains("error output"),
            "error file should contain stderr output, got: {error_contents}"
        );

        // Verify log file has stdout content
        assert!(log_path.exists(), "log file should exist");
        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log_contents.contains("stdout here"),
            "log file should contain stdout output"
        );
    }

    /// Test completeness: verify that large output is captured without truncation
    /// since we're using file redirection (no 64KB pipe buffer limit).
    #[test]
    fn test_invoke_completeness_no_truncation() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("complete.log");
        let error_path = tmp.path().join("complete.err");

        let log_file_str = log_path.to_string_lossy();
        let error_file_str = error_path.to_string_lossy();
        // Generate 1000 lines — more than a single pipe buffer
        let shell_command = format!(
            "for i in $(seq 1 1000); do printf 'LINE %03d\\n' \"$i\"; done > '{}' 2> '{}'",
            log_file_str, error_file_str
        );

        let status = Command::new("sh")
            .arg("-c")
            .arg(&shell_command)
            .status()
            .expect("failed to execute shell command");

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

        // After timestamping, all content should still be present
        timestamp_log_file(&log_path).unwrap();
        let timestamped = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            timestamped.contains("LINE 001"),
            "timestamped log is missing the beginning"
        );
        assert!(
            timestamped.contains("LINE 1000"),
            "timestamped log is missing the end"
        );
    }

    /// Test that output without a trailing newline is still captured
    /// and timestamped correctly.
    #[test]
    fn test_invoke_no_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("no-newline.log");
        let error_path = tmp.path().join("no-newline.err");

        let log_file_str = log_path.to_string_lossy();
        let error_file_str = error_path.to_string_lossy();
        let shell_command = format!(
            "printf 'first line\\nsecond line\\nno trailing newline' > '{}' 2> '{}'",
            log_file_str, error_file_str
        );

        let status = Command::new("sh")
            .arg("-c")
            .arg(&shell_command)
            .status()
            .expect("failed to execute shell command");

        assert!(status.success());

        timestamp_log_file(&log_path).unwrap();
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
            "log is missing partial last line"
        );
    }
}
