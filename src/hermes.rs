use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

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
}

/// Invoke `hermes chat` with the given arguments.
///
/// Always passes `--yolo`. If `work_dir` is provided, hermes runs from that
/// directory (which becomes its project root).
///
/// `issue_tag` is a short identifier like `owner/repo#123` — it replaces the
/// old generic `[hermes]` log prefix so that interleaved output from multiple
/// concurrent issues can be told apart. The profile name is also included,
/// making the prefix `[<profile> <issue_tag>]`.
///
/// Streams stdout/stderr to `tracing::info!` / `tracing::error!` line by line.
/// Returns `Ok(())` on exit code 0.
/// On non-zero exit: writes captured stderr to `error_file` and returns `Err`.
pub fn invoke(args: &InvokeArgs, issue_tag: &str) -> Result<()> {
    let mut cmd = Command::new("hermes");
    if let Some(dir) = &args.work_dir {
        cmd.current_dir(dir);
    }
    cmd.arg("chat")
        .arg("-p")
        .arg(&args.prompt)
        .arg("--yolo")
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
        .map_err(|e| {
            anyhow::anyhow!(
                "[{} {}] failed to spawn hermes: {}",
                args.profile,
                issue_tag,
                e
            )
        })?;

    let stderr_capture: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let log_prefix = format!("[{} {}]", args.profile, issue_tag);

    // Drain stderr on a dedicated thread (concurrent with stdout drain)
    let stderr_stream = child.stderr.take();
    let stderr_capture_clone = Arc::clone(&stderr_capture);
    let log_prefix_stderr = log_prefix.clone();
    let stderr_thread = std::thread::spawn(move || {
        if let Some(stderr) = stderr_stream {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        tracing::error!("{} stderr: {}", log_prefix_stderr, l);
                        let mut cap = stderr_capture_clone
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        cap.push_str(&l);
                        cap.push('\n');
                    }
                    Err(e) => tracing::warn!("{} stderr read error: {}", log_prefix_stderr, e),
                }
            }
        }
    });

    // Drain stdout in this thread
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => tracing::info!("{} {}", log_prefix, l),
                Err(e) => tracing::warn!("{} stdout read error: {}", log_prefix, e),
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
        anyhow::bail!(
            "[{} {}] hermes exited with code {}",
            args.profile,
            issue_tag,
            code
        );
    }
}
