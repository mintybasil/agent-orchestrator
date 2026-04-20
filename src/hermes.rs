use anyhow::{bail, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

/// Invoke hermes with a single prompt argument.
/// Streams stdout/stderr to tracing::info!/tracing::error! line by line.
/// Returns Ok(()) on exit code 0.
/// On non-zero exit: writes captured stderr to `error_file` and returns Err.
pub fn invoke(prompt: &str, error_file: &Path) -> Result<()> {
    let mut child = Command::new("hermes")
        .arg(prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn hermes: {}", e))?;

    // Stream stdout
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => tracing::info!("[hermes] {}", l),
                Err(e) => tracing::warn!("[hermes stdout read error] {}", e),
            }
        }
    }

    // Capture stderr separately (read after stdout done)
    let stderr_bytes = if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let mut lines_text = String::new();
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    tracing::error!("[hermes stderr] {}", l);
                    lines_text.push_str(&l);
                    lines_text.push('\n');
                }
                Err(e) => tracing::warn!("[hermes stderr read error] {}", e),
            }
        }
        lines_text
    } else {
        String::new()
    };

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(-1);
        // Write stderr to error file
        if let Err(e) = std::fs::write(error_file, &stderr_bytes) {
            tracing::warn!("failed to write error file {:?}: {}", error_file, e);
        }
        bail!("hermes exited with code {}", code);
    }
}
