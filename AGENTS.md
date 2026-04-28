# AGENTS.md — agent-orchestrator

Guidance for AI coding agents (Claude Code, Codex, Hermes, etc.) working in
this repository.

## Build and test commands

```
# Build
cargo build --release

# Lint
cargo clippy

# Run tests (unit tests in src/template.rs and src/workflow.rs)
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
  hermes.rs     -- Subprocess invoker for the hermes CLI agent
  poller.rs     -- tokio poll loop, concurrency dedup, JSON persistence
  runner.rs     -- Per-issue sequential step executor
  template.rs   -- {{key}} placeholder renderer + unit tests
  workflow.rs   -- Step and Hook types; loaded by config.rs
config.example.toml  -- Annotated example config (copy to config.toml and edit)
data/                -- Runtime data dir (gitignored); created on first run
```

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

Hermes is launched from inside the `workspace/` directory with `--worktree`,
so it operates on an up-to-date checkout of `main`.

## Architecture notes

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

## Hermes invocation

Each step calls `hermes chat` with the following flags:

| Flag | Source | Required |
|---|---|---|
| `-p <prompt>` | Rendered `prompt_template` | always |
| `--yolo` | hardcoded | always |
| `--profile <name>` | `profile` field on step | always |
| `--worktree` | `worktree = true` on step | optional |
| `--provider <name>` | `provider` field on step | optional |
| `--model <name>` | `model` field on step | optional |

Before the first step runs, the orchestrator clones the target repo into
`<data-dir>/<owner>/<repo>/workspace/` (or pulls latest `main` if it already
exists). Hermes is invoked from inside that `workspace/` directory so it
treats the repo as its project root.

## Extending the workflow

Workflow steps live in your config file as `[[steps]]` tables — no recompile
needed. Edit the config and restart the daemon.

To run a different workflow, run a separate daemon instance pointing at a
different config file:
`agent-orchestrator --config other-config.toml`

### Step format

```toml
[[steps]]
name = "my-step"
prompt_template = "Do something for {{owner}}/{{repo}} issue {{issue_number}}. Write output to {{output_path}}/my-step.md."
profile = "cto"         # required: passed to hermes as --profile
# worktree = true       # optional: passes --worktree to hermes
# provider = "openai"   # optional: passes --provider to hermes
# model = "o3"          # optional: passes --model to hermes

# Optional pre-hooks (run before hermes)
[[steps.pre_hooks]]
type = "script"
command = "scripts/validate.sh"
args = ["{{issue_number}}"]

# Optional post-hooks (run after hermes)
[[steps.post_hooks]]
type = "file_not_empty"
path = "{{output_path}}/my-step.md"
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
| `file_not_empty` | `path` (string, supports placeholders) | Fail if file is absent or zero bytes |
| `script` | `command` (string), `args` (array of strings, support placeholders) | Spawn process; fail on non-zero exit |

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
message. The stderr capture for the failing hermes invocation is written to
`<data-dir>/{owner}/{repo}/{issue_number}/step_NN_<name>.error`.

To retry a failed issue, restart the daemon (the `permanently_failed` set is
in-memory only).
