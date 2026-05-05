# agent-orchestrator

A lightweight Rust daemon that polls GitHub for open issues assigned to a
configured user and runs a multi-step agent workflow against each one using
[Hermes](https://github.com/mintybasil/hermes) (or any compatible CLI agent).

## How it works

1. On each poll tick the daemon calls the GitHub Issues API for every configured
   repo, filtering by assignee and `state=open`.
2. New issues (not yet completed or in-flight) are dispatched concurrently as
   tokio tasks, gated by an optional concurrency limit.
3. Each issue runs through the configured workflow steps sequentially by invoking
   `hermes chat` as a subprocess with a rendered prompt.
4. Step outputs are written to `<data-dir>/{owner}/{repo}/{issue_number}/` and
   validated (non-empty file check) before the next step starts.
5. Completed issue keys are persisted to `<data-dir>/completed.json` so they survive
   restarts. Failed issues are written to `<data-dir>/failed.json` with a timestamp
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

Place one or more `.toml` workflow config files in a directory and point
`agent-orchestrator` at it with `--workflows`. Each file is loaded as an
independent workflow.

Copy `config.example.toml` into your workflows directory and edit it:

```toml
[[triggers]]
type = "github_issue_assigned"
assigned_to = "your-github-username"
allowed_users = ["your-github-username"]

[[repos]]
owner = "your-org"
repo  = "your-repo"

[[steps]]
name = "triage"
prompt_template = "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}. Write a triage summary to {{output_path}}/triage.md."
harness = { type = "hermes", profile = "cto" }

[[steps]]
name = "implement"
prompt_template = "Read the triage at {{output_path}}/step_00_triage.md. Implement the changes described. Write a summary to {{output_path}}/step_01_implement.md."
harness = { type = "hermes", profile = "cto" }
```

### Git configuration

```toml
[git]
clone = true           # Clone/pull the repo (default: true)
worktree = false       # Per-issue worktrees (default: false; requires clone = true)
default_branch = "main" # Branch for pull/worktree (default: "main")
```

### Step fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Human-readable step name (used in log output and error filenames) |
| `prompt_template` | string | yes | Prompt sent to hermes; supports `{{placeholders}}` |
| `harness` | table | yes | Agent harness config; `type = "hermes"` with `profile`, optional `provider` and `model` |

### Hermes invocation

Each step runs:

```
hermes chat -p <prompt> --yolo --quiet --profile <profile> [--provider <provider>] [--model <model>]
```

### Template placeholders

| Placeholder | Value |
|---|---|
| `{{owner}}` | Repository owner |
| `{{repo}}` | Repository name |
| `{{issue_number}}` | GitHub issue number |
| `{{output_path}}` | Full path where this step must write its output (under `<data-dir>/{owner}/{repo}/{issue_number}/`) |
| `{{repo_path}}` | Path to the base repo clone; empty when `git.clone = false` |

### Hooks

Steps support optional `pre_hooks` and `post_hooks`:

```toml
[[steps.pre_hooks]]
type = "script"
command = "scripts/validate.sh"
args = ["{{issue_number}}"]
```

```toml
[[steps.post_hooks]]
type = "file_not_empty"
path = "{{output_path}}"

[[steps.post_hooks]]
type = "push_code"
```

| Hook type        | Fields            | Effect                                                                |
|------------------|-------------------|-----------------------------------------------------------------------|
| `file_not_empty` | `path`            | Fail if file is absent or zero bytes                                  |
| `script`         | `command`, `args` | Spawn process; fail on non-zero exit                                  |
| `push_code`      | N/A               | Push any unpushed commits to the remote; fail if no new commits exist |

## Running

```
export GITHUB_TOKEN=***
./target/release/agent-orchestrator --workflows /path/to/workflows/
```

### CLI flags

| Flag | Default | Description |
|---|---|---|
| `--workflows <DIR>` | `.` | Directory containing workflow `.toml` files |
| `--limit <N>` | `0` | Max concurrent workflow runs (0 = unlimited) |
| `--interval <SECS>` | `60` | Poll interval in seconds |
| `--data-dir <DIR>` | `~/.agent-orchestrator` | Data directory for logs and state |
| `--show-logs` | off | Print harness stdout/stderr to terminal in addition to log files |

The daemon logs to stdout via `tracing`; set `RUST_LOG=debug` for verbose output.

On startup the daemon validates:

- The workflows directory contains at least one `.toml` file.
- All config files parse correctly and define at least one trigger and one step.
- `GITHUB_TOKEN` is set and non-empty.
- The data directory exists or can be created, and is writable.
- `hermes` is present on `PATH`.

Any validation failure exits with a descriptive error message.

## Data directory layout

```
<data-dir>/
├── completed.json              # Array of "owner/repo/N" keys
├── failed.json                # Array of {key, timestamp, error} objects
└── {owner}/{repo}/
    ├── repo/                  # git clone of the repo (when git.clone = true)
    └── {issue_number}/
        ├── worktree-{N}/        # per-issue git worktree (when git.worktree = true)
        ├── step_NN_<name>.log   # Full harness stdout+stderr log (always written)
        ├── step_NN_<name>.error # stderr on failure only
        └── step_NN_<name>.md    # Step output files written by hermes
```

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `GITHUB_TOKEN` | — | GitHub API auth (required) |
| `RUST_LOG` | `info` | Log level passed to `tracing-subscriber` |

## License

MIT