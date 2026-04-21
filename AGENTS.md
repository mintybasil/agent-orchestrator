# AGENTS.md — agent-orchestrator

Guidance for AI coding agents (Claude Code, Codex, Hermes, etc.) working in
this repository.

## Build and test commands

```
# Build
cargo build --release

# Lint
cargo clippy

# Run tests (3 unit tests in src/template.rs)
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
  main.rs       -- Entry point: startup validation, tracing init, poll loop
  config.rs     -- Config struct (TOML) + clap CLI (--config flag)
  github.rs     -- GitHub Issues API: paginated list_assigned_issues()
  hermes.rs     -- Subprocess invoker for the hermes CLI agent
  poller.rs     -- tokio poll loop, concurrency dedup, JSON persistence
  runner.rs     -- Per-issue sequential step executor
  template.rs   -- {{key}} placeholder renderer + unit tests
  workflow.rs   -- Hardcoded 2-step workflow definition
config.toml     -- Sample config (do not commit real tokens or org names)
data/           -- Runtime data dir (gitignored); created on first run
```

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

## Extending the workflow

All workflow steps live in `src/workflow.rs` in the `workflow()` function.
To add a step:

1. Append a `Step` struct to the returned `Vec`.
2. Set `output_file` to a unique filename (e.g. `step_02_pr.md`).
3. Use `{{step_N_output}}` (0-indexed) in `prompt_template` to reference a
   prior step's output path.
4. Set `validation: Validation::FileNonEmpty` (currently the only variant).

The runner wires prior-step output paths into the template context
automatically, in order.

## PR and branching rules

- NEVER commit directly to `main`. Always branch + PR.
- Branch naming convention: `feat/<description>`, `fix/<description>`,
  `docs/<description>`, `chore/<description>`.
- Remove plan or scratch files before opening a PR.
- Keep image names in the `zerokrab` org namespace.

## Key runtime requirements

- `GITHUB_TOKEN` env var must be set (validated on startup, hard exit if missing).
- `hermes` must be on `PATH` (validated on startup, hard exit if missing).
- `data/` must be writable (validated on startup, hard exit if not).
- Config file must be readable TOML (validated on startup, hard exit if not).

## Adding a new validation type

`Validation` is an enum in `src/workflow.rs`. To add a new variant:

1. Add the variant to the `Validation` enum.
2. Add a match arm in `runner.rs` under the `match step.validation` block.
3. Add a unit test in `src/template.rs` or a new test module as appropriate.

## Debugging a failed issue

Failed issues are written to `data/failed.json` with a timestamp and error
message. The stderr capture for the failing hermes invocation is written to
`data/{owner}/{repo}/{issue_number}/step_NN_<name>.error`.

To retry a failed issue, restart the daemon (the `permanently_failed` set is
in-memory only).
