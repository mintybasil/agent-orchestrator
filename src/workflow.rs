use serde::Deserialize;

/// A hook that runs before or after a step.
///
/// Deserialized from TOML using the `type` key as a discriminant:
/// `{ type = "file_not_empty", path = "..." }` or
/// `{ type = "script", command = "...", args = ["..."] }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hook {
    /// Assert that the file at `path` exists and contains at least one byte.
    /// `path` may contain template placeholders (e.g. `"{{output_path}}"`).
    FileNotEmpty { path: String },

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
    /// Hooks that run *before* the hermes invocation.
    #[serde(default)]
    pub pre_hooks: Vec<Hook>,
    /// Hooks that run *after* a successful hermes invocation.
    #[serde(default)]
    pub post_hooks: Vec<Hook>,
    /// Optional hermes profile name passed via `--profile <name>`.
    pub profile: String,
    /// When true, passes `--worktree` to hermes.
    #[serde(default)]
    pub worktree: bool,
    /// Optional provider passed to hermes via `--provider <provider>`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional model passed to hermes via `--model <model>`.
    #[serde(default)]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Helper: parse a Config from a TOML string with boilerplate headers.
    fn parse_config(steps_toml: &str) -> anyhow::Result<Config> {
        let full = format!(
            "poll_interval_secs = 60\nassigned_to = \"test\"\nallowed_issue_creators = [\"test\"]\n\n[[repos]]\nowner = \"o\"\nrepo = \"r\"\n\n{}",
            steps_toml
        );
        // Write to a temp file and load via Config::load
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(full.as_bytes()).unwrap();
        Config::load(f.path())
    }

    #[test]
    fn file_not_empty_hook_deserializes() {
        let steps = r#"
[[steps]]
name = "triage"
prompt_template = "Do triage for {{owner}}/{{repo}}. Output: {{output_path}}."
profile = "test"

[[steps.post_hooks]]
type = "file_not_empty"
path = "{{output_path}}"
"#;
        let config = parse_config(steps).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert_eq!(config.steps[0].name, "triage");
        assert_eq!(config.steps[0].post_hooks.len(), 1);
        assert!(matches!(
            config.steps[0].post_hooks[0],
            Hook::FileNotEmpty { .. }
        ));
    }

    #[test]
    fn script_hook_deserializes() {
        let steps = r#"
[[steps]]
name = "lint"
prompt_template = "Lint the code."
profile = "test"

[[steps.post_hooks]]
type = "script"
command = "cargo"
args = ["clippy"]
"#;
        let config = parse_config(steps).unwrap();
        assert_eq!(config.steps.len(), 1);
        match &config.steps[0].post_hooks[0] {
            Hook::Script { command, args } => {
                assert_eq!(command, "cargo");
                assert_eq!(args, &["clippy"]);
            }
            other => panic!("unexpected hook: {:?}", other),
        }
    }

    #[test]
    fn empty_steps_errors() {
        // Config::load rejects configs with no [[steps]]
        let err = parse_config("").unwrap_err();
        assert!(err.to_string().contains("no [[steps]]"));
    }

    #[test]
    fn malformed_toml_errors() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"not valid toml ][[\n").unwrap();
        assert!(Config::load(f.path()).is_err());
    }

    #[test]
    fn missing_file_errors() {
        assert!(Config::load(std::path::Path::new("/nonexistent/config.toml")).is_err());
    }
}
