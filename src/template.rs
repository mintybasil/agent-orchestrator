use std::collections::HashMap;

/// Render a prompt template by substituting {{key}} placeholders.
/// Unknown placeholders are left as-is.
pub fn render(template: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        let placeholder = format!("{{{{{}}}}}", key);
        out = out.replace(&placeholder, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_substitution() {
        let mut vars = HashMap::new();
        vars.insert("owner", "zerokrab".to_string());
        vars.insert("repo", "bento-hancho".to_string());
        vars.insert("issue_number", "42".to_string());
        let tmpl = "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}.";
        let result = render(tmpl, &vars);
        assert_eq!(result, "Read GitHub issue #42 in zerokrab/bento-hancho.");
    }

    #[test]
    fn unknown_placeholder_left_as_is() {
        let vars = HashMap::new();
        let result = render("hello {{unknown}}", &vars);
        assert_eq!(result, "hello {{unknown}}");
    }

    #[test]
    fn output_path_substitution() {
        let mut vars = HashMap::new();
        vars.insert("output_path", "data/owner/repo/42".to_string());
        let result = render("Write triage to {{output_path}}/triage.md.", &vars);
        assert_eq!(result, "Write triage to data/owner/repo/42/triage.md.");
    }

    #[test]
    fn output_path_is_absolute_when_data_root_is_absolute() {
        use std::path::PathBuf;
        // Simulate how runner.rs builds issue_dir (data_root is always absolute after Task 2)
        let data_root = PathBuf::from("/home/user/.agent-orchestrator");
        let issue_dir = data_root.join("owner").join("repo").join("42");
        assert!(
            issue_dir.is_absolute(),
            "issue_dir must be absolute: {:?}",
            issue_dir
        );
        // Simulate the template substitution
        let mut vars = std::collections::HashMap::new();
        let output_path_str = issue_dir.to_string_lossy().into_owned();
        vars.insert("output_path", output_path_str.clone());
        let result = render("Write to {{output_path}}/plan.md", &vars);
        // Extract the substituted output_path portion and verify it is absolute
        let expected_prefix = format!("Write to {}/", output_path_str);
        assert!(
            result.starts_with(&expected_prefix),
            "rendered output_path must be absolute: expected prefix {:?}, got {:?}",
            expected_prefix,
            result
        );
    }

    #[test]
    fn workspace_substitution() {
        let mut vars = HashMap::new();
        vars.insert("workspace", "/data/owner/repo/workspace".to_string());
        let result = render("Work in {{workspace}} for this issue.", &vars);
        assert_eq!(result, "Work in /data/owner/repo/workspace for this issue.");
    }
}
