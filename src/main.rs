use crate::config::Cli;
use clap::Parser;

mod askpass;
mod config;
mod git;
mod github;
mod hermes;
mod poller;
mod runner;
mod template;
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
        "agent-orchestrator starting: {} repos, {} workflow steps, poll every {}s, assigned_to={}, data_dir={}",
        config.repos.len(),
        workflow_steps.len(),
        config.poll_interval_secs,
        config.assigned_to,
        data_root.display()
    );

    let completed = poller::load_completed(&data_root);

    if let Err(e) = poller::run_poll_loop(
        config,
        token,
        &data_root,
        completed,
        workflow_steps,
        &current_exe,
    )
    .await
    {
        tracing::error!("poll loop exited with error: {}", e);
        std::process::exit(1);
    }
}
