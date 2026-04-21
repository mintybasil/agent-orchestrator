use serde::Deserialize;

/// A hook that runs before or after a step.
///
/// # Variants
///
/// * `FileNonEmpty { path }` — assert that a file exists and is non-empty.
///   Use `"{{output_path}}"` as the path to check the step's own output.
///   Template placeholders are resolved before the check runs.
///
/// * `Script { command, args }` — run an arbitrary executable.
///   The process inherits the runner's environment; stdout/stderr are streamed
///   to tracing. A non-zero exit code is treated as hook failure.
///   Template placeholders inside `args` strings are resolved before execution.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hook {
    /// Assert that the file at `path` exists and contains at least one byte.
    /// `path` may contain template placeholders (e.g. `"{{output_path}}"`).
    FileNonEmpty { path: String },

    /// Spawn an external process.
    ///
    /// `command` is the executable name or absolute path.
    /// `args` are the arguments; each element may contain template placeholders.
    Script { command: String, args: Vec<String> },
}

/// A single step in the agent workflow.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub name: String,
    pub prompt_template: String,
    pub output_file: String,
    /// Hooks that run *before* the hermes invocation.
    #[serde(default)]
    pub pre_hooks: Vec<Hook>,
    /// Hooks that run *after* a successful hermes invocation.
    #[serde(default)]
    pub post_hooks: Vec<Hook>,
}

/// Top-level shape of a workflow TOML file.
#[derive(Debug, Deserialize)]
pub struct WorkflowFile {
    pub steps: Vec<Step>,
}

/// Load and validate a workflow TOML file.
///
/// Returns `Err` if the file cannot be read, cannot be parsed, or contains no steps.
pub fn load(path: &std::path::Path) -> anyhow::Result<Vec<Step>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read workflow file {:?}: {}", path, e))?;
    let wf: WorkflowFile = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("failed to parse workflow file {:?}: {}", path, e))?;
    anyhow::ensure!(!wf.steps.is_empty(), "workflow file {:?} contains no steps", path);
    Ok(wf.steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_valid_workflow() {
        let toml = r#"
[[steps]]
name = "triage"
prompt_template = "Do triage for {{owner}}/{{repo}} issue {{issue_number}}. Output: {{output_path}}."
output_file = "step_00_triage.md"

[[steps.post_hooks]]
type = "file_non_empty"
path = "{{output_path}}"
"#;
        let f = write_temp(toml);
        let steps = load(f.path()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "triage");
        assert_eq!(steps[0].output_file, "step_00_triage.md");
        assert_eq!(steps[0].post_hooks.len(), 1);
        assert!(matches!(steps[0].post_hooks[0], Hook::FileNonEmpty { .. }));
    }

    #[test]
    fn load_script_hook() {
        let toml = r#"
[[steps]]
name = "lint"
prompt_template = "Lint the code."
output_file = "step_00_lint.md"

[[steps.post_hooks]]
type = "script"
command = "cargo"
args = ["clippy"]
"#;
        let f = write_temp(toml);
        let steps = load(f.path()).unwrap();
        assert_eq!(steps.len(), 1);
        match &steps[0].post_hooks[0] {
            Hook::Script { command, args } => {
                assert_eq!(command, "cargo");
                assert_eq!(args, &["clippy"]);
            }
            other => panic!("unexpected hook: {:?}", other),
        }
    }

    #[test]
    fn load_empty_steps_errors() {
        let toml = "steps = []\n";
        let f = write_temp(toml);
        let err = load(f.path()).unwrap_err();
        assert!(err.to_string().contains("no steps"));
    }

    #[test]
    fn load_malformed_toml_errors() {
        let f = write_temp("not valid toml ][[\n");
        assert!(load(f.path()).is_err());
    }

    #[test]
    fn load_missing_file_errors() {
        let result = load(std::path::Path::new("/nonexistent/workflow.toml"));
        assert!(result.is_err());
    }
}
