# AGENTS.md — agent-orchestrator

Guidance for AI coding agents (Claude Code, Codex, Hermes, etc.) working in
this repository.

## Build and test commands

```
# Build
cargo build --release

# Lint
cargo clippy

# Run tests (unit tests in src/template.rs, src/hooks.rs, src/workflow.rs, etc.)
cargo test

# Type-check only (faster than a full build)
cargo check
```

> NOTE: If `cargo test` OOMs the linker, set:
> `CARGO_TARGET_DIR=~/cargo-target TMPDIR=~/build-tmp cargo test`
> Do NOT put these env vars in GitHub Actions workflow files — they are
> local agent workarounds only.

## Repository layout

```
src/
  main.rs       -- Entry point: askpass dispatch, startup validation, tracing init, poll loop
  askpass.rs    -- ASKPASS handler: responds to git credential prompts via re-invocation
  config.rs     -- Config struct (TOML) + clap CLI (--config flag)
  git.rs        -- Git workspace management: clone/pull with ASKPASS auth
  github.rs     -- GitHub Issues API: paginated list_assigned_issues()
  harness.rs    -- Pluggable agent harness trait + HarnessConfig enum (each variant carries its own options)
  hermes.rs     -- Harness impl for the hermes CLI agent; also exposes low-level invoke()
  hooks.rs      -- Hook enum + run_hook() dispatcher; pre/post step checks
  poller.rs     -- tokio poll loop using Trigger trait, concurrency dedup, JSON persistence
  runner.rs     -- Per-issue sequential step executor (uses Harness + hooks)
  template.rs   -- {{key}} placeholder renderer + unit tests
  trigger.rs    -- Generalized trigger trait + TriggerConfig enum
  workflow.rs   -- Step type (harness-agnostic; harness-specific options live in HarnessConfig)
config.example.toml  -- Annotated example config (copy to config.toml and edit)
data/                -- Runtime data dir (gitignored); created on first run
```

## Architecture (v0.2)

### Triggers

Triggers define **what initiates a workflow run**. They implement the `Trigger`
trait and are specified in config via `[[triggers]]` tables.

Currently supported:
- `github_issue_assigned` — polls GitHub for issues assigned to a user
- `github_pr_review` — polls GitHub for PR reviews/comments by allowed users

Adding a new trigger type:
1. Add a variant to `TriggerConfig` in `src/trigger.rs`
2. Add a struct implementing `Trigger`
3. Add a match arm in `TriggerConfig::build()`

### Hooks

Hooks are pre/post step checks. They live in `src/hooks.rs`.

Adding a new hook type:
1. Add a variant to the `Hook` enum in `src/hooks.rs`
2. Add a match arm in `run_hook()`

### Agent Harnesses

Harnesses define **which agent backend runs a step**. They implement the
`Harness` trait and are specified per-step via the `harness` field.

Each `HarnessConfig` variant carries **harness-specific options** — the Step
struct is harness-agnostic. For example, `HarnessConfig::Hermes` carries
`profile`, `worktree`, `provider`, and `model` because those are hermes CLI
flags, not generic step concerns.

Currently supported:
- `hermes` — invokes the hermes CLI agent

Adding a new harness:
1. Add a variant to `HarnessConfig` in `src/harness.rs` (with its specific fields)
2. Add a struct implementing `Harness`
3. Add a match arm in `HarnessConfig::build()`
4. (Optional) Add a startup validation in `main.rs`

## Data directory layout

Each monitored repo gets a workspace directory under the data dir:

```
{data-dir}/
  {owner}/{repo}/
    workspace/           -- git clone of the repo (auto-managed)
    {issue_number}/      -- per-issue output directory
  completed.json         -- set of completed issue keys
  failed.json            -- list of failed issue entries
```

Before each workflow run, the orchestrator ensures the workspace exists:
- **First run**: `git clone` into the workspace directory using `GIT_ASKPASS` for authentication
- **Subsequent runs**: `git pull origin main` to update to latest (also via `GIT_ASKPASS`)

### Git authentication (GIT_ASKPASS)

The binary acts as its own credential helper. When it spawns `git clone` or `git pull`,
it sets three environment variables on the child process:

| Env var | Value | Purpose |
|---|---|---|
| `GIT_ASKPASS` | Path to the running binary (`current_exe`) | Tells git to re-invoke the binary for credential prompts |
| `AO_ASKPASS_MODE` | `1` | Sentinel: puts the re-invocation into askpass mode |
| `AO_GIT_TOKEN` | The `GITHUB_TOKEN` value | The token to return as the password |
| `GIT_TERMINAL_PROMPT` | `0` | Prevents interactive auth fallback |

When git needs credentials, it re-invokes the binary. `main()` checks for
`AO_ASKPASS_MODE` first — if set, it runs the askpass handler (which prints
either `x-access-token` for username prompts or the token for password prompts)
and exits immediately, bypassing all normal startup.

**Why this approach**: The token is never embedded in URLs, never written to
`.git/config`, never appears in process arguments, and never leaks outside the
process's env block. The ASKPASS round-trip is scoped to the git child process
only.

## Additional architecture notes

**Concurrency model**: Each eligible issue is dispatched as a tokio task.
`in_flight: HashSet<String>` prevents double-dispatch within a tick.
`permanently_failed: HashSet<String>` prevents within-run retry of failed
issues (they are only retried on daemon restart, per spec). Both sets and
`completed: HashSet<String>` are wrapped in `Arc<Mutex<_>>`.

**File write safety**: `completed.json` and `failed.json` are protected by a
shared `file_lock: Arc<Mutex<()>>`. Writes are atomic: content is written to
a `.tmp` file then renamed into place.

**Pipe deadlock prevention**: `hermes.rs` drains `stderr` on a dedicated
`std::thread` concurrently with the main thread draining `stdout`. This avoids
deadlock when the subprocess writes enough output to fill the OS pipe buffer
on both streams simultaneously.

**GitHub pagination**: `list_assigned_issues()` loops with a `page` counter
until GitHub returns an empty page. `per_page=100` minimises round trips.

## Hermes invocation (HermesHarness)

When a step uses `harness = { type = "hermes", ... }`, it calls `hermes chat`:

| Flag | Source | Required |
|---|---|---|
| `-p <prompt>` | Rendered `prompt_template` | always |
| `--yolo` | hardcoded | always |
| `--profile <name>` | `profile` field in HarnessConfig::Hermes | always |
| `--worktree` | `worktree = true` in HarnessConfig::Hermes | optional |
| `--provider <name>` | `provider` field in HarnessConfig::Hermes | optional |
| `--model <name>` | `model` field in HarnessConfig::Hermes | optional |

Before the first step runs, the orchestrator clones the target repo into
`<data-dir>/<owner>/<repo>/workspace/` (or pulls latest `main` if it already
exists). The hermes harness is invoked from inside that `workspace/` directory
so it treats the repo as its project root.

## Extending the workflow

Workflow steps live in your config file as `[[steps]]` tables — no recompile
needed. Edit the config and restart the daemon.

To run a different workflow, run a separate daemon instance pointing at a
different config file:
`agent-orchestrator --config other-config.toml`

### Trigger format

```toml
[[triggers]]
type = "github_issue_assigned"
assigned_to = "your-github-username"
allowed_users = ["your-github-username"]

# [[triggers]]
# type = "github_pr_review"
# allowed_users = ["your-github-username"]
```

### Step format

```toml
[[steps]]
name = "my-step"
prompt_template = "Do something for {{owner}}/{{repo}} issue {{issue_number}}. Write output to {{output_path}}/my-step.md."
harness = { type = "hermes", profile = "cto" }
# harness = { type = "hermes", profile = "cto", worktree = true, provider = "openai", model = "o3" }

# Optional pre-hooks (run before the agent harness)
[[steps.pre_hooks]]
type = "script"
command = "scripts/validate.sh"
args = ["{{issue_number}}"]

# Optional post-hooks (run after the agent harness)
[[steps.post_hooks]]
type = "file_non_empty"
path = "{{output_path}}/my-step.md"

# Optional: ensure committed code is pushed to the remote
[[steps.post_hooks]]
type = "push_code"
```

### Template placeholders

| Placeholder | Value |
|---|---|
| `{{owner}}` | Repository owner |
| `{{repo}}` | Repository name |
| `{{issue_number}}` | GitHub issue number |
| `{{output_path}}` | Path to the issue data directory (`<data-dir>/<owner>/<repo>/<issue_number>/`), created before the first step runs |
| `{{workspace}}` | Path to the git clone of the repo (`<data-dir>/<owner>/<repo>/workspace/`), auto-managed by the orchestrator |

### Hook types

| `type` | Fields | Effect |
|---|---|---|
| `file_non_empty` | `path` (string, supports placeholders) | Fail if file is absent or zero bytes |
| `script` | `command` (string), `args` (array of strings, support placeholders) | Spawn process; fail on non-zero exit |
| `push_code` | _(none)_ | Push any unpushed commits to the remote; fail if no new commits exist |

Hooks run in declaration order. A failure aborts the step and marks the issue as failed.

## PR and branching rules

- NEVER commit directly to `main`. Always branch + PR.
- Branch naming convention: `feat/<description>`, `fix/<description>`,
  `docs/<description>`, `chore/<description>`.
- Remove plan or scratch files before opening a PR.
- Keep image names in the `zerokrab` org namespace.

## Key runtime requirements

- `GITHUB_TOKEN` env var must be set (validated on startup, hard exit if missing).
- `hermes` must be on `PATH` (validated on startup, hard exit if missing).
- `<data-dir>` must be writable (validated on startup, hard exit if not); override with `--data-dir` (default: `~/.agent-orchestrator`).
- Config file must be readable TOML (validated on startup, hard exit if not).

## Debugging a failed issue

Failed issues are written to `<data-dir>/failed.json` with a timestamp and error
message. The stderr capture for the failing harness invocation is written to
`<data-dir>/{owner}/{repo}/{issue_number}/step_NN_<name>.error`.

To retry a failed issue, restart the daemon (the `permanently_failed` set is
in-memory only).