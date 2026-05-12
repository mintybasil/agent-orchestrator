//! Hook system — pre/post step hooks.
//!
//! Hooks are small checks or actions that run before/after a workflow step.
//! Adding a new hook type = add an enum variant + a match arm in `run_hook`.

use crate::template::render;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

/// A hook that runs before or after a step.
///
/// Deserialized from TOML using the `type` key as a discriminant:
/// `{ type = "file_not_empty", path = "..." }` or
/// `{ type = "script", command = "...", args = ["..."] }`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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

/// Execute a single hook, resolving any template placeholders in its arguments.
///
/// On failure writes a human-readable message to `error_path` and returns Err.
pub fn run_hook(hook: &Hook, vars: &HashMap<String, String>, error_path: &Path) -> Result<()> {
    match hook {
        Hook::FileNotEmpty { path: raw_path } => {
            let path = render(raw_path, vars);
            match fs::metadata(&path) {
                Ok(m) if m.len() > 0 => Ok(()),
                Ok(_) => {
                    let msg = format!("hook FileNotEmpty: file is empty: {}", path);
                    let _ = fs::write(error_path, &msg);
                    anyhow::bail!("{}", msg);
                }
                Err(e) => {
                    let msg = format!("hook FileNotEmpty: file missing ({}): {}", path, e);
                    let _ = fs::write(error_path, &msg);
                    anyhow::bail!("{}", msg);
                }
            }
        }

        Hook::Script { command, args } => {
            let resolved_args: Vec<String> = args.iter().map(|a| render(a, vars)).collect();

            tracing::info!(command, args = resolved_args.join(" "), "running hook");

            let mut child = Command::new(command)
                .args(&resolved_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    anyhow::anyhow!("hook Script: failed to spawn `{}`: {}", command, e)
                })?;

            // Stream stderr on a dedicated thread (avoids pipe deadlock).
            let stderr_stream = child.stderr.take();
            let stderr_thread = std::thread::spawn(move || {
                if let Some(stderr) = stderr_stream {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        tracing::error!(stderr = true, "{}", line);
                    }
                }
            });

            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    tracing::info!("{}", line);
                }
            }

            let _ = stderr_thread.join();
            let status = child.wait()?;

            if status.success() {
                Ok(())
            } else {
                let code = status.code().unwrap_or(-1);
                let msg = format!("hook Script: `{}` exited with code {}", command, code);
                let _ = fs::write(error_path, &msg);
                anyhow::bail!("{}", msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn file_not_empty_hook_deserializes() {
        let toml = r#"
type = "file_not_empty"
path = "{{output_path}}"
"#;
        let hook: Hook = toml::from_str(toml).unwrap();
        assert!(matches!(hook, Hook::FileNotEmpty { .. }));
    }

    #[test]
    fn script_hook_deserializes() {
        let toml = r#"
type = "script"
command = "cargo"
args = ["clippy"]
"#;
        let hook: Hook = toml::from_str(toml).unwrap();
        match hook {
            Hook::Script { command, args } => {
                assert_eq!(command, "cargo");
                assert_eq!(args, vec!["clippy"]);
            }
            other => panic!("unexpected hook: {:?}", other),
        }
    }

    #[test]
    fn file_not_empty_succeeds_for_nonempty_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"content").unwrap();
        let path = f.path().to_string_lossy().into_owned();

        let hook = Hook::FileNotEmpty { path };
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("output_path".to_string(), "/tmp".to_string());
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path()).is_ok());
    }

    #[test]
    fn file_not_empty_fails_for_missing_file() {
        let hook = Hook::FileNotEmpty {
            path: "/nonexistent/file.xyz".to_string(),
        };
        let vars: HashMap<String, String> = HashMap::new();
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path()).is_err());
    }

    #[test]
    fn file_not_empty_fails_for_empty_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_string_lossy().into_owned();

        let hook = Hook::FileNotEmpty { path };
        let vars: HashMap<String, String> = HashMap::new();
        let error_path = tempfile::NamedTempFile::new().unwrap();
        assert!(run_hook(&hook, &vars, error_path.path()).is_err());
    }

    #[test]
    fn hook_rejects_unknown_fields() {
        let toml = r#"
type = "file_not_empty"
path = "{{output_path}}"
typo = "oops"
"#;
        let result = toml::from_str::<Hook>(toml);
        assert!(
            result.is_err(),
            "expected unknown field to be rejected, got: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field"),
            "error should mention 'unknown field', got: {err}"
        );
    }
}
