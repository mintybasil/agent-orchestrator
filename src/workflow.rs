use serde::Deserialize;

use crate::harness::HarnessConfig;

/// A single step in the agent workflow.
///
/// Steps are harness-agnostic: they only define *what* to do (name,
/// prompt, hooks) and *which* harness to use. Harness-specific options
/// (e.g. hermes profile, worktree, provider, model) live inside the
/// `harness` field.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub name: String,
    pub prompt_template: String,
    /// Hooks that run *before* the agent invocation.
    #[serde(default)]
    pub pre_hooks: Vec<crate::hooks::Hook>,
    /// Hooks that run *after* a successful agent invocation.
    #[serde(default)]
    pub post_hooks: Vec<crate::hooks::Hook>,
    /// Agent harness to use for this step, including harness-specific options.
    pub harness: HarnessConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Helper: parse a Config from a TOML string with boilerplate headers.
    fn parse_config(steps_toml: &str) -> anyhow::Result<Config> {
        let full = format!(
            "poll_interval_secs = 60\n\
             [[triggers]]\n\
             type = \"github_issue_assigned\"\n\
             assigned_to = \"test\"\n\
             allowed_users = [\"test\"]\n\n\
             [[repos]]\n\
             owner = \"o\"\n\
             repo = \"r\"\n\n\
             {}",
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
harness = { type = "hermes", profile = "test" }

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
            crate::hooks::Hook::FileNotEmpty { .. }
        ));
    }

    #[test]
    fn script_hook_deserializes() {
        let steps = r#"
[[steps]]
name = "lint"
prompt_template = "Lint the code."
harness = { type = "hermes", profile = "test" }

[[steps.post_hooks]]
type = "script"
command = "cargo"
args = ["clippy"]
"#;
        let config = parse_config(steps).unwrap();
        assert_eq!(config.steps.len(), 1);
        match &config.steps[0].post_hooks[0] {
            crate::hooks::Hook::Script { command, args } => {
                assert_eq!(command, "cargo");
                assert_eq!(args, &["clippy"]);
            }
            other => panic!("unexpected hook: {:?}", other),
        }
    }

    #[test]
    fn push_code_hook_deserializes() {
        let steps = r#"
[[steps]]
name = "triage"
prompt_template = "Do triage."
harness = { type = "hermes", profile = "test" }

[[steps.post_hooks]]
type = "push_code"
"#;
        let config = parse_config(steps).unwrap();
        assert_eq!(config.steps.len(), 1);
        assert!(matches!(
            config.steps[0].post_hooks[0],
            crate::hooks::Hook::PushCode
        ));
    }

    #[test]
    fn step_hermes_harness_with_profile() {
        let steps = r#"
[[steps]]
name = "triage"
prompt_template = "Do triage."
harness = { type = "hermes", profile = "cto" }
"#;
        let config = parse_config(steps).unwrap();
        match &config.steps[0].harness {
            HarnessConfig::Hermes {
                profile,
                provider,
                model,
            } => {
                assert_eq!(profile, "cto");
                assert!(provider.is_none());
                assert!(model.is_none());
            }
        }
    }

    #[test]
    fn empty_steps_errors() {
        // Config::load rejects configs with no [[steps]]
        // We need valid triggers but empty steps
        use std::io::Write;
        let toml = r#"
poll_interval_secs = 60

[[triggers]]
type = "github_issue_assigned"
assigned_to = "test"
allowed_users = ["test"]

[[repos]]
owner = "o"
repo = "r"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();
        let err = Config::load(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("steps"), "expected 'steps' in error: {msg}");
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
