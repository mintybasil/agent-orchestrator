use crate::config::Cli;
use clap::Parser;

mod config;
mod github;
mod hermes;
mod poller;
mod runner;
mod template;
mod workflow;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Load config
    let config = match config::Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERROR: failed to load config {:?}: {}", cli.config, e);
            std::process::exit(1);
        }
    };

    let workflow_steps = config.steps.clone();

    // Init tracing (after config so RUST_LOG is readable)
    tracing_subscriber::fmt()
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

    // data/ directory writable
    let data_root = std::path::PathBuf::from("data");
    if let Err(e) = std::fs::create_dir_all(&data_root) {
        eprintln!("ERROR: cannot create data/ directory: {}", e);
        std::process::exit(1);
    }
    let probe_path = data_root.join(".probe");
    if let Err(e) = std::fs::write(&probe_path, b"") {
        eprintln!("ERROR: data/ directory is not writable: {}", e);
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&probe_path);

    // hermes on PATH
    let hermes_check = std::process::Command::new("which").arg("hermes").output();
    match hermes_check {
        Ok(out) if out.status.success() => {}
        _ => {
            eprintln!("ERROR: `hermes` binary not found on PATH");
            std::process::exit(1);
        }
    }

    tracing::info!(
        "agent-orchestrator starting: {} repos, {} workflow steps, poll every {}s, assigned_to={}",
        config.repos.len(),
        workflow_steps.len(),
        config.poll_interval_secs,
        config.assigned_to
    );

    let completed = poller::load_completed(&data_root);

    if let Err(e) = poller::run_poll_loop(config, token, data_root, completed, workflow_steps).await
    {
        tracing::error!("poll loop exited with error: {}", e);
        std::process::exit(1);
    }
}
