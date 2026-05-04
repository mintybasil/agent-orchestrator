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

    #[test]
    fn pr_number_substitution() {
        let mut vars = HashMap::new();
        vars.insert("pr_number", "123".to_string());
        vars.insert("owner", "acme".to_string());
        vars.insert("repo", "project".to_string());
        let result = render("Review PR #{{pr_number}} in {{owner}}/{{repo}}.", &vars);
        assert_eq!(result, "Review PR #123 in acme/project.");
    }

    #[test]
    fn trigger_specific_vars_merge_with_globals() {
        // Simulates the merging logic from runner::run_event:
        // global vars (owner, repo, output_path, workspace) + trigger vars
        let mut vars: HashMap<&str, String> = [
            ("owner", "acme".to_string()),
            ("repo", "project".to_string()),
            ("output_path", "/data/acme/project/42".to_string()),
            ("workspace", "/data/acme/project/workspace".to_string()),
        ]
        .into_iter()
        .collect();

        // Merge trigger-specific variables (simulating a PR trigger)
        let trigger_vars =
            std::collections::HashMap::from([("pr_number".to_string(), "7".to_string())]);
        for (k, v) in &trigger_vars {
            vars.insert(k.as_str(), v.clone());
        }

        let result = render(
            "Review PR #{{pr_number}} in {{owner}}/{{repo}}. Write to {{output_path}}.",
            &vars,
        );
        assert_eq!(
            result,
            "Review PR #7 in acme/project. Write to /data/acme/project/42."
        );
    }
}
