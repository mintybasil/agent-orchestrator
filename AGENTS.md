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
  config.rs     -- Config struct (TOML) + clap CLI (--workflows / --limit / --interval flags); includes GitConfig
  git.rs        -- Git repo/worktree management: clone/pull, worktree create/remove, push, ASKPASS auth
  github.rs     -- GitHub Issues API: paginated list_assigned_issues()
  harness.rs    -- Pluggable agent harness trait + HarnessConfig enum (each variant carries its own options)
  hermes.rs     -- Harness impl for the hermes CLI agent; also exposes low-level invoke()
  hooks.rs      -- Hook enum + run_hook() dispatcher; pre/post step checks
  poller.rs     -- tokio poll loop using Trigger trait, concurrency dedup, capped concurrency (Semaphore), JSON persistence, multi-workflow support, hot-reload workflow configs via mtime scanning
  runner.rs     -- Per-issue sequential step executor (uses Harness + hooks + worktree lifecycle)
  template.rs   -- {{key}} placeholder renderer + unit tests
  trigger.rs    -- Generalized trigger trait + TriggerConfig enum; EventKey, TriggerEvent, LocalFileTrigger
  workflow.rs   -- Step type (harness-agnostic; harness-specific options live in HarnessConfig)
config.example.toml  -- Annotated example config (copy to config.toml and edit)
data/                -- Runtime data dir (gitignored); created on first run
```

## Architecture (v0.2)

### Git configuration (GitConfig)

Git behavior is controlled by the `[git]` config section:

```toml
[git]
clone = true           # Whether to clone/pull the repo (default: true)
worktree = false       # Whether to create a per-issue worktree (default: false)
default_branch = "main" # Branch for git pull and worktree creation (default: "main")
```

**Validation**: `worktree = true` requires `clone = true` (enforced on startup).

When `git.worktree = true`, the orchestrator creates a fresh git worktree for
each issue before running steps, and cleans it up afterward:

1. **Before steps**: `git worktree add -b <branch> <path> <default_branch>` creates a unique branch (`ao/<event-label>-<timestamp>`) and checks it out at `<data-dir>/<owner>/<repo>/<issue_number>/worktree-<N>`. Each worktree gets its own branch to avoid git's restriction against multiple checkouts of the same branch.
2. **After steps**: cleanup runs:
   - If uncommitted changes exist → error, worktree left for manual inspection
   - If unpushed commits exist → push them, then remove worktree + delete branch
   - If clean → remove worktree with `git worktree remove --force` and delete the branch with `git branch -D`

### Triggers

Triggers define **what initiates a workflow run**. They implement the `Trigger`
trait and are specified in config via `[[triggers]]` tables.

Currently supported:
- `github_issue_assigned` — polls GitHub for issues assigned to a user
- `github_pr_review` — polls GitHub for PR reviews/comments by allowed users
- `local_file` — watches a local directory for files matching a glob pattern

Each trigger implementation owns its own credentials (injected at construction
time via `TriggerConfig::build()`). The `Trigger::poll()` method is fully
agnostic — no GitHub-specific arguments are passed. This means new event
sources (cron, webhooks, etc.) can be added without modifying core
poller/dispatcher logic.

### TriggerEvent and EventKey

`TriggerEvent` is the uniform event struct produced by all triggers. It
carries:

| Field | Purpose |
|---|---|
| `owner` / `repo` | Repository identifiers (or `"local"` for non-GitHub triggers) |
| `key` | Opaque dedup string (e.g. `"42"`, `"99/1234567"`) |
| `workspace_id` | Directory name under the data dir (e.g. `"42"`, `"99_review-1234567"`) |
| `number` | Numeric ID (0 when not applicable) |
| `label` | Human-readable label for logging |
| `variables` | Trigger-specific template variables (merged into step prompts) |

`EventKey` is derived from `TriggerEvent` via `to_event_key()` and is used by
the runner to construct workspace paths and template variables. `EventKey` is
defined in `src/trigger.rs` (its canonical location) and re-exported from
`runner.rs` for backward compatibility.

Adding a new trigger type:
1. Add a variant to `TriggerConfig` in `src/trigger.rs`
2. Add a struct implementing `Trigger` (own any credentials as fields)
3. Add a match arm in `TriggerConfig::build()` (inject token/credentials here)
4. Add a match arm in any exhaustive `TriggerConfig` matches (e.g. `config.rs` tests)

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
`profile`, `provider`, `model`, and `max_turns` because those are hermes CLI flags, not
generic step concerns.

**Note**: Worktree management is handled by the orchestrator (via `[git]`
config), not by individual harness steps. The `--worktree` flag has been removed
from the hermes harness.

Currently supported:
- `hermes` — invokes the hermes CLI agent

Adding a new harness:
1. Add a variant to `HarnessConfig` in `src/harness.rs` (with its specific fields)
2. Add a struct implementing `Harness`
3. Add a match arm in `HarnessConfig::build()`
4. (Optional) Add a startup validation in `main.rs`

### Hot-reload

The poll loop watches the `--workflows` directory for changes to `.toml` files.
On each tick, it compares file modification times (mtimes) against the previously
recorded state. If files were added, removed, or modified, all configs are
reparsed and `WorkflowEntry` structs are rebuilt.

Key functions in `src/poller.rs`:
- `load_workflow_entries()` — parses all `.toml` files from the workflows directory,
  builds `WorkflowEntry` list + file mtime map
- `load_workflow_entries_if_changed()` — compares current directory state against
  a previous mtime snapshot; returns `Reload(new_entries, new_state)`, `Unchanged`,
  or `Error`
- `ReloadDecision` enum — models the three outcomes of a hot-reload check

**In-flight workflows are unaffected**: Running tasks hold `Arc<>` references
which are immutable. A config reload creates new entries; existing spawned
tasks continue with their original steps/repos/git_config.

**Error resilience**: If reparsing fails (e.g., a malformed TOML file), the
previous valid config is kept and an error is logged. The daemon never crashes
due to a bad config file being dropped into the workflows directory.

**Startup validation**: On startup, `Config::load_all()` still runs to fail fast
on invalid configs. The poll loop's hot-reload is only for subsequent changes
after the daemon is already running.

## Data directory layout

Each monitored repo gets a directory under the data dir:

```
{data-dir}/
  {owner}/{repo}/
    repo/                -- git clone of the repo (auto-managed, when git.clone = true)
    {workspace_id}/      -- per-event output directory
      worktree-{N}/        -- per-event git worktree (when git.worktree = true)
      step_NN_<name>.log   -- full harness stdout+stderr log
      step_NN_<name>.error -- stderr on failure only
      step_NN_<name>.prompt -- rendered prompt text (after template substitution)
  completed.json         -- set of completed event keys
  failed.json            -- list of failed event entries
```

For issues, `workspace_id` is the issue number (e.g. `42`).
For PR reviews, `workspace_id` includes the review ID to ensure each review
event gets its own discrete directory (e.g. `99_review-1234567`).
For local file triggers, `workspace_id` is derived from the file stem (e.g.
`plan` for a file named `plan.md`).

Before each workflow run, the orchestrator ensures the repo exists:
- **First run**: `git clone` into the `repo/` directory using `GIT_ASKPASS` for authentication
- **Subsequent runs**: `git pull origin <default_branch>` to update to latest (also via `GIT_ASKPASS`)

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

**Concurrency model**: Each eligible event is dispatched as a tokio task.
`in_flight: HashSet<String>` prevents double-dispatch within a tick.
`permanently_failed: HashSet<String>` prevents within-run retry of failed
events (they are only retried on daemon restart, per spec). Both sets and
`completed: HashSet<String>` are wrapped in `Arc<Mutex<_>>`.

A `tokio::sync::Semaphore` (permit count from `--limit`) caps the number of
concurrent workflow runs across all workflows. When `--limit` is 0 (default),
the semaphore is effectively unlimited. Each spawned task acquires a permit
before executing; the permit is held until the task completes.

**File write safety**: `completed.json` and `failed.json` are protected by a
shared `file_lock: Arc<Mutex<()>>`. Writes are atomic: content is written to
a `.tmp` file then renamed into place.

**Pipe deadlock prevention**: `hermes.rs` drains `stderr` on a dedicated
`std::thread` concurrently with the main thread draining `stdout`. This avoids
deadlock when the subprocess writes enough output to fill the OS pipe buffer
on both streams simultaneously. Both streams are written to a per-step log
file (`step_NN_<name>.log`); when `--show-logs` is set, they are also printed
via tracing.

**GitHub pagination**: `list_assigned_issues()` loops with a `page` counter
until GitHub returns an empty page. `per_page=100` minimises round trips.

## Hermes invocation (HermesHarness)

When a step uses `harness = { type = "hermes", ... }`, it calls `hermes chat`:

| Flag | Source | Required |
|---|---|---|
| `-p <prompt>` | Rendered `prompt_template` | always |
| `--yolo` | hardcoded | always |
| `--quiet` | hardcoded | always |
| `--profile <name>` | `profile` field in HarnessConfig::Hermes | always |
| `--provider <name>` | `provider` field in HarnessConfig::Hermes | optional |
| `--model <name>` | `model` field in HarnessConfig::Hermes | optional |
| `--max-turns <n>` | `max_turns` field in HarnessConfig::Hermes | optional |

The working directory for the harness invocation depends on `[git]` config:
- **worktree = true**: invoked from the per-issue worktree directory
- **clone = true, worktree = false**: invoked from the repo checkout (`repo/`)
- **clone = false**: invoked from the per-issue output directory

Before the first step runs, the orchestrator clones the target repo into
`<data-dir>/<owner>/<repo>/repo/` (or pulls latest if it already exists).

## Extending the workflow

Workflow steps live in your config file as `[[steps]]` tables — no recompile
needed. Edit the config and the daemon picks up changes automatically on the
next poll tick; no restart required.

To run additional workflows, add more `.toml` files to the `--workflows`
directory. Each file is loaded as an independent workflow config:

```bash
agent-orchestrator --workflows /path/to/workflows/ --limit 4 --interval 30
```

### Git config format

```toml
[git]
clone = true           # Whether to clone/pull the repo (default: true)
worktree = false       # Per-issue worktrees (default: false; requires clone = true)
default_branch = "main" # Branch for pull/worktree (default: "main")
```

### Trigger format

```toml
[[triggers]]
type = "github_issue_assigned"
assigned_to = "your-github-username"
allowed_users = ["your-github-username"]

# [[triggers]]
# type = "github_pr_review"
# allowed_users = ["your-github-username"]

# [[triggers]]
# type = "local_file"
# path = "/path/to/watch"
# pattern = "*.md"   # optional glob filter (default: "*")
```

### Step format

```toml
[[steps]]
name = "my-step"
prompt_template = "Do something for {{owner}}/{{repo}} issue {{issue_number}}. Write output to {{output_path}}/my-step.md."
harness = { type = "hermes", profile = "cto" }
# harness = { type = "hermes", profile = "cto", provider = "openai", model = "o3", max_turns = 10 }

# Optional pre-hooks (run before the agent harness)
[[steps.pre_hooks]]
type = "script"
command = "scripts/validate.sh"
args = ["{{issue_number}}"]

# Optional post-hooks (run after the agent harness)
[[steps.post_hooks]]
type = "file_not_empty"
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
| `{{default_branch}}` | Default branch from git config (e.g. `main`) |
| `{{output_path}}` | Path to the event data directory (`<data-dir>/<owner>/<repo>/<workspace_id>/`), created before the first step runs |
| `{{repo_path}}` | Path to the base repository clone (`<data-dir>/<owner>/<repo>/repo/`); empty string when `git.clone = false` |

Trigger-specific placeholders are merged into the template variables at
runtime. Available variables depend on the trigger type:

| Trigger | Extra placeholders |
|---|---|
| `github_issue_assigned` | `{{issue_number}}` |
| `github_pr_review` | `{{pr_number}}` |
| `local_file` | `{{file_name}}`, `{{file_path}}` |

### Hook types

| `type` | Fields | Effect |
|---|---|---|
| `file_not_empty` | `path` (string, supports placeholders) | Fail if file is absent or zero bytes |
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
- `--workflows` directory must contain at least one `.toml` file (validated on startup, hard exit if not).
- `--limit` caps concurrent workflow runs; 0 means unlimited (default).
- `--interval` sets the poll interval in seconds; defaults to 60.
- `--show-logs` flag: when set, harness output is printed to the terminal in addition to being written to log files. Log files are always written.

## Debugging a failed event

Failed events are written to `<data-dir>/failed.json` with a timestamp and error
message. The stderr capture for the failing harness invocation is written to
`<data-dir>/{owner}/{repo}/{workspace_id}/step_NN_<name>.error`.

To retry a failed event, restart the daemon (the `permanently_failed` set is
in-memory only).