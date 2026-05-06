use crate::config::Cli;
use chrono::Local;
use clap::Parser;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Custom timer for short timestamps in log output: `HH:MM:SS`
/// instead of the default RFC 3339 with nanoseconds.
struct ShortTime;

impl FormatTime for ShortTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Local::now().format("%H:%M:%S"))
    }
}

mod askpass;
mod config;
mod git;
mod github;
mod harness;
mod hermes;
mod hooks;
mod poller;
mod runner;
mod template;
mod trigger;
mod workflow;

#[tokio::main]
async fn main() {
    // Askpass mode: if the binary is being re-invoked by git for credentials,
    // handle it immediately — no logging, no config, no network.
    if std::env::var(askpass::ASKPASS_MODE_ENV).is_ok() {
        let args: Vec<String> = std::env::args().collect();
        let exit_code = askpass::run(&args);
        std::process::exit(exit_code);
    }

    let cli = Cli::parse();

    // Load all workflow configs from the --workflows directory
    let configs = match config::Config::load_all(&cli.workflows) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    };

    // Use the compact formatter so span fields (profile, issue, step_name)
    // appear on every event line, making it easy to tell which issue
    // produced each log line when multiple run concurrently.
    // Short timestamps: HH:MM:SS instead of full RFC 3339 with nanoseconds.
    tracing_subscriber::fmt()
        .compact()
        .with_timer(ShortTime)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // GITHUB_TOKEN
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("ERROR: GITHUB_TOKEN environment variable is not set or empty");
            std::process::exit(1);
        }
    };

    // Resolve current executable path for GIT_ASKPASS
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: cannot determine current executable path: {}", e);
            std::process::exit(1);
        }
    };

    // data/ directory writable
    let data_root: std::path::PathBuf = {
        let raw = cli.data_dir.unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".agent-orchestrator")
        });
        if let Err(e) = std::fs::create_dir_all(&raw) {
            eprintln!("ERROR: cannot create data dir {:?}: {}", raw, e);
            std::process::exit(1);
        }
        match raw.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ERROR: cannot resolve data dir {:?}: {}", raw, e);
                std::process::exit(1);
            }
        }
    };
    let probe_path = data_root.join(".probe");
    if let Err(e) = std::fs::write(&probe_path, b"") {
        eprintln!("ERROR: data/ directory is not writable: {}", e);
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&probe_path);

    // Validate that default harness (hermes) is available on PATH.
    // Only check hermes since it's the built-in default harness;
    // future harnesses will validate themselves.
    let hermes_check = std::process::Command::new("which").arg("hermes").output();
    match hermes_check {
        Ok(out) if out.status.success() => {}
        _ => {
            eprintln!("ERROR: `hermes` binary not found on PATH");
            std::process::exit(1);
        }
    }

    let concurrency_msg = if cli.limit == 0 {
        "unlimited".to_string()
    } else {
        cli.limit.to_string()
    };

    tracing::info!(
        "agent-orchestrator starting: workflows={}, poll interval={}s, concurrent={}, data_dir={}",
        configs.len(),
        cli.interval,
        concurrency_msg,
        data_root.display()
    );

    let completed = poller::load_completed(&data_root);

    // Shutdown signal: first Ctrl+C / SIGTERM → graceful shutdown (stop
    // dispatching, drain active workflows); second signal → immediate exit.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        let mut first_signal = true;
        loop {
            tokio::select! {
                _ = sigint.recv() => {
                    if first_signal {
                        tracing::info!("SIGINT received. Gracefully shutting down…");
                        let _ = shutdown_tx.send(true);
                        first_signal = false;
                    } else {
                        tracing::warn!("Second shutdown signal received. Forcing immediate exit.");
                        std::process::exit(1);
                    }
                }
                _ = sigterm.recv() => {
                    if first_signal {
                        tracing::info!("SIGTERM received. Gracefully shutting down…");
                        let _ = shutdown_tx.send(true);
                        first_signal = false;
                    } else {
                        tracing::warn!("Second SIGTERM received. Forcing immediate exit.");
                        std::process::exit(1);
                    }
                }
            }
        }
    });

    if let Err(e) = poller::run_poll_loop(
        configs,
        token,
        &data_root,
        completed,
        &current_exe,
        cli.show_logs,
        cli.limit,
        cli.interval,
        shutdown_rx,
    )
    .await
    {
        tracing::error!("poll loop exited with error: {}", e);
        std::process::exit(1);
    }
}
