use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Invoke `hermes chat` with the given prompt, always passing `--yolo`.
/// Passes `--profile <profile>` and, if `worktree` is true, `--worktree`.
/// Optionally passes `--provider <provider>` and `--model <model>`.
/// The prompt is passed via `-p <prompt>`.
/// If `work_dir` is provided, hermes runs from that directory (which becomes its project root).
/// Streams stdout/stderr to tracing::info!/tracing::error! line by line.
/// Returns Ok(()) on exit code 0.
/// On non-zero exit: writes captured stderr to `error_file` and returns Err.
pub fn invoke(
    prompt: &str,
    profile: &str,
    worktree: bool,
    provider: Option<&str>,
    model: Option<&str>,
    error_file: &Path,
    work_dir: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new("hermes");
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    cmd.arg("chat")
        .arg("-p")
        .arg(prompt)
        .arg("--yolo")
        .arg("--profile")
        .arg(profile);
    if worktree {
        cmd.arg("--worktree");
    }
    if let Some(provider) = provider {
        cmd.arg("--provider").arg(provider);
    }
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn hermes: {}", e))?;

    let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // Drain stderr on a dedicated thread (concurrent with stdout drain)
    let stderr_stream = child.stderr.take();
    let stderr_capture_clone = Arc::clone(&stderr_capture);
    let stderr_thread = std::thread::spawn(move || {
        if let Some(stderr) = stderr_stream {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        tracing::error!("[hermes stderr] {}", l);
                        let mut cap = stderr_capture_clone
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        cap.push_str(&l);
                        cap.push('\n');
                    }
                    Err(e) => tracing::warn!("[hermes stderr read error] {}", e),
                }
            }
        }
    });

    // Drain stdout in this thread
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => tracing::info!("[hermes] {}", l),
                Err(e) => tracing::warn!("[hermes stdout read error] {}", e),
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
        if let Err(e) = std::fs::write(error_file, &stderr_text) {
            tracing::warn!("failed to write error file {:?}: {}", error_file, e);
        }
        anyhow::bail!("hermes exited with code {}", code);
    }
}
