# agent-orchestrator

A lightweight Rust daemon that polls GitHub for open issues assigned to a
configured user and runs a multi-step agent workflow against each one using
[Hermes](https://github.com/mintybasil/hermes) (or any compatible CLI agent).

## How it works

1. On each poll tick the daemon calls the GitHub Issues API for every configured
   repo, filtering by assignee and `state=open`.
2. New issues (not yet completed or in-flight) are dispatched concurrently as
   tokio tasks.
3. Each issue runs through the configured workflow — currently two steps:
   **triage** then **implement** — by invoking `hermes` as a subprocess with a
   rendered prompt.
4. Step outputs are written to `data/{owner}/{repo}/{issue_number}/` and
   validated (non-empty file check) before the next step starts.
5. Completed issue keys are persisted to `data/completed.json` so they survive
   restarts. Failed issues are written to `data/failed.json` with a timestamp
   and error message, and are not retried within the same daemon run.

## Requirements

- Rust 1.75+ (2021 edition)
- `hermes` binary on `PATH`
- A GitHub personal access token with `repo` read scope

## Building

```
cargo build --release
```

The binary is at `target/release/agent-orchestrator`.

## Configuration

Copy and edit `config.toml`:

```toml
poll_interval_secs = 60
assigned_to = "your-github-username"

[[repos]]
owner = "your-org"
repo  = "your-repo"

[[repos]]
owner = "your-org"
repo  = "another-repo"
```

All fields are required. `assigned_to` is matched against the GitHub assignee
login for open issues.

## Running

```
export GITHUB_TOKEN=ghp_...
./target/release/agent-orchestrator --config config.toml
```

The `--config` flag defaults to `config.toml` in the current directory. The
daemon logs to stdout via `tracing`; set `RUST_LOG=debug` for verbose output.

On startup the daemon validates:

- The config file is readable and parses correctly.
- `GITHUB_TOKEN` is set and non-empty.
- The `data/` directory exists or can be created, and is writable.
- `hermes` is present on `PATH`.

Any validation failure exits with a descriptive error message.

## Data directory layout

```
data/
├── completed.json              # Array of "owner/repo/N" keys
├── failed.json                 # Array of {key, timestamp, error} objects
└── {owner}/{repo}/{issue_number}/
    ├── step_00_triage.md       # Triage step output
    ├── step_01_implement.md    # Implement step output
    └── step_NN_<name>.error    # Stderr capture on failure (if any)
```

## Workflow steps

Steps are defined in `src/workflow.rs`. Each step receives a rendered prompt
with the following template variables:

| Variable | Description |
|---|---|
| `{{owner}}` | Repository owner |
| `{{repo}}` | Repository name |
| `{{issue_number}}` | GitHub issue number |
| `{{output_path}}` | Full path where this step must write its output |
| `{{step_N_output}}` | Full path to step N's output file (for chaining) |

To add a step, append a `Step` struct to the `workflow()` function in
`src/workflow.rs`.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `GITHUB_TOKEN` | — | GitHub API auth (required) |
| `RUST_LOG` | `info` | Log level passed to `tracing-subscriber` |

## License

MIT
