// Template placeholders:
// {{owner}}          - repository owner
// {{repo}}           - repository name
// {{issue_number}}   - GitHub issue number
// {{output_path}}    - full path to this step's output file
// {{step_0_output}}  - full path to step 0's output file (pattern: step_N_output for any prior step)

/// A hook that runs before or after a step.
// Script is intentional public API for workflow authors — suppress the dead_code lint.
#[allow(dead_code)]
///
/// Hooks are executed in order. If any hook fails the step is aborted and the
/// issue is marked as failed — identical behaviour to the old `Validation`
/// failure path.
///
/// # Variants
///
/// * `FileNonEmpty(path)` — assert that a file exists and is non-empty.
///   Use `"{{output_path}}"` as the path to check the step's own output.
///   Template placeholders are resolved before the check runs.
///
/// * `Script { command, args }` — run an arbitrary executable.
///   The process inherits the runner's environment; stdout/stderr are streamed
///   to tracing. A non-zero exit code is treated as hook failure.
///   Template placeholders inside `args` strings are resolved before execution.
#[derive(Debug, Clone)]
pub enum Hook {
    /// Assert that the file at `path` exists and contains at least one byte.
    /// `path` may contain template placeholders (e.g. `"{{output_path}}"`).
    FileNonEmpty(String),

    /// Spawn an external process.
    ///
    /// `command` is the executable name or absolute path.
    /// `args` are the arguments; each element may contain template placeholders.
    Script { command: String, args: Vec<String> },
}

/// A single step in the agent workflow.
#[derive(Debug, Clone)]
pub struct Step {
    pub name: &'static str,
    pub prompt_template: &'static str,
    pub output_file: &'static str,
    /// Hooks that run *before* the hermes invocation.
    /// Typical use: lint inputs, assert preconditions.
    pub pre_hooks: Vec<Hook>,
    /// Hooks that run *after* a successful hermes invocation.
    /// Typical use: validate output, run linters on generated files.
    pub post_hooks: Vec<Hook>,
}

/// Returns the hardcoded workflow.
pub fn workflow() -> Vec<Step> {
    vec![
        Step {
            name: "triage",
            prompt_template: "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}. \
                              Write a triage summary to {{output_path}}.",
            output_file: "step_00_triage.md",
            pre_hooks: vec![],
            post_hooks: vec![Hook::FileNonEmpty("{{output_path}}".to_string())],
        },
        Step {
            name: "implement",
            prompt_template: "Read the triage at {{step_0_output}}. \
                              Implement the changes described. \
                              Write a summary of what you did to {{output_path}}.",
            output_file: "step_01_implement.md",
            pre_hooks: vec![],
            post_hooks: vec![Hook::FileNonEmpty("{{output_path}}".to_string())],
        },
    ]
}
