# agent-orchestrator

A lightweight Rust daemon that polls GitHub for open issues assigned to a
configured user and runs a multi-step agent workflow against each one using
[Hermes](https://github.com/mintybasil/hermes) (or any compatible CLI agent).

## How it works

1. On each poll tick the daemon calls the GitHub Issues API for every configured
   repo, filtering by assignee and `state=open`.
2. New issues (not yet completed or in-flight) are dispatched concurrently as
   tokio tasks.
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

Copy `config.example.toml` to `config.toml` and edit it:

```
poll_interval_secs = 60
assigned_to = "your-github-username"
allowed_issue_creators = ["your-github-username"]

[[repos]]
owner = "your-org"
repo  = "your-repo"

[[steps]]
name = "triage"
prompt_template = "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}. Write a triage summary to {{output_path}}/triage.md."
profile = "cto"

[[steps]]
name = "implement"
prompt_template = "Read the triage at {{step_0_output}}. Implement the changes described. Write a summary to {{output_path}}/implement.md."
profile = "cto"
```

### Step fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Human-readable step name (used in log output and error filenames) |
| `prompt_template` | string | yes | Prompt sent to hermes; supports `{{placeholders}}` |
| `profile` | string | yes | Hermes profile passed via `--profile` |
| `worktree` | bool | no | When `true`, passes `--worktree` to hermes (default: `false`) |
| `provider` | string | no | Passed to hermes via `--provider` |
| `model` | string | no | Passed to hermes via `--model` |

### Hermes invocation

Each step runs:

```
hermes chat -p <prompt> --yolo --profile <profile> [--worktree] [--provider <provider>] [--model <model>]
```

### Template placeholders

| Placeholder | Value |
|---|---|
| `{{owner}}` | Repository owner |
| `{{repo}}` | Repository name |
| `{{issue_number}}` | GitHub issue number |
| `{{output_path}}` | Full path where this step must write its output (under `<data-dir>/{owner}/{repo}/{issue_number}/`) |
| `{{step_N_output}}` | Full path to step N's output file (0-indexed; for chaining steps) |

### Hooks

Steps support optional `pre_hooks` and `post_hooks`:

```toml
[[steps.post_hooks]]
type = "file_non_empty"
path = "{{output_path}}"

[[steps.pre_hooks]]
type = "script"
command = "scripts/validate.sh"
args = ["{{issue_number}}"]
```

| Hook type | Fields | Effect |
|---|---|---|
| `file_non_empty` | `path` | Fail if file is absent or zero bytes |
| `script` | `command`, `args` | Spawn process; fail on non-zero exit |

## Running

```
export GITHUB_TOKEN=***
./target/release/agent-orchestrator --config config.toml
```

The `--config` flag defaults to `config.toml` in the current directory.
Use `--data-dir <DIR>` to override the default data directory (`~/.agent-orchestrator`).
The daemon logs to stdout via `tracing`; set `RUST_LOG=debug` for verbose output.

On startup the daemon validates:

- The config file is readable and parses correctly.
- `GITHUB_TOKEN` is set and non-empty.
- The data directory (`--data-dir`, default `~/.agent-orchestrator`) exists or can be created, and is writable.
- `hermes` is present on `PATH`.

Any validation failure exits with a descriptive error message.

## Data directory layout

```
<data-dir>/
├── completed.json              # Array of "owner/repo/N" keys
├── failed.json                # Array of {key, timestamp, error} objects
└── {owner}/{repo}/{issue_number}/
    └── step_NN_<name>.md      # Step output files written by hermes
```

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `GITHUB_TOKEN` | — | GitHub API auth (required) |
| `RUST_LOG` | `info` | Log level passed to `tracing-subscriber` |

## License

MIT
