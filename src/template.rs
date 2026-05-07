use std::collections::HashMap;

/// Render a prompt template by substituting {{key}} placeholders.
/// Unknown placeholders are left as-is.
pub fn render(template: &str, vars: &HashMap<String, String>) -> String {
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
        vars.insert("owner".to_string(), "zerokrab".to_string());
        vars.insert("repo".to_string(), "bento-hancho".to_string());
        vars.insert("issue_number".to_string(), "42".to_string());
        let tmpl = "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}.";
        let result = render(tmpl, &vars);
        assert_eq!(result, "Read GitHub issue #42 in zerokrab/bento-hancho.");
    }

    #[test]
    fn unknown_placeholder_left_as_is() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = render("hello {{unknown}}", &vars);
        assert_eq!(result, "hello {{unknown}}");
    }

    #[test]
    fn output_path_substitution() {
        let mut vars = HashMap::new();
        vars.insert("output_path".to_string(), "data/owner/repo/42".to_string());
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
        let mut vars: HashMap<String, String> = HashMap::new();
        let output_path_str = issue_dir.to_string_lossy().into_owned();
        vars.insert("output_path".to_string(), output_path_str.clone());
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
    fn repo_path_substitution() {
        let mut vars = HashMap::new();
        vars.insert("repo_path".to_string(), "/data/owner/repo/repo".to_string());
        let result = render("Work in {{repo_path}} for this issue.", &vars);
        assert_eq!(result, "Work in /data/owner/repo/repo for this issue.");
    }

    #[test]
    fn default_branch_substitution() {
        let mut vars = HashMap::new();
        vars.insert("default_branch".to_string(), "develop".to_string());
        let result = render("The default branch is {{default_branch}}.", &vars);
        assert_eq!(result, "The default branch is develop.");
    }

    #[test]
    fn repo_path_is_absolute_when_clone_is_true() {
        use std::path::PathBuf;
        // Simulate how runner.rs builds repo_dir: data_root (absolute) +
        // owner/repo/repo
        let data_root = PathBuf::from("/home/user/.agent-orchestrator");
        let repo_dir = data_root.join("owner").join("repo").join("repo");
        assert!(
            repo_dir.is_absolute(),
            "repo_dir must be absolute: {:?}",
            repo_dir
        );
        // Verify the template variable would carry the absolute path
        let mut vars = HashMap::new();
        vars.insert("repo_path".to_string(), repo_dir.to_string_lossy().into_owned());
        let result = render("cd {{repo_path}}", &vars);
        assert!(
            result.starts_with("cd /"),
            "repo_path should resolve to absolute path: got {:?}",
            result
        );
    }

    #[test]
    fn repo_path_is_empty_string_when_clone_is_false() {
        // When git.clone = false, repo_dir is None and the template var is ""
        let mut vars = HashMap::new();
        vars.insert("repo_path".to_string(), String::new());
        let result = render("Work in {{repo_path}}done", &vars);
        assert_eq!(result, "Work in done");
    }

    #[test]
    fn pr_number_substitution() {
        let mut vars = HashMap::new();
        vars.insert("pr_number".to_string(), "123".to_string());
        vars.insert("owner".to_string(), "acme".to_string());
        vars.insert("repo".to_string(), "project".to_string());
        let result = render("Review PR #{{pr_number}} in {{owner}}/{{repo}}.", &vars);
        assert_eq!(result, "Review PR #123 in acme/project.");
    }

    #[test]
    fn trigger_specific_vars_merge_with_globals() {
        // Simulates the merging logic from runner::run_event:
        // global vars (owner, repo, default_branch, output_path, repo_path) + trigger vars
        let mut vars: HashMap<String, String> = [
            ("owner".into(), "acme".to_string()),
            ("repo".into(), "project".to_string()),
            ("default_branch".into(), "main".to_string()),
            ("output_path".into(), "/data/acme/project/42".to_string()),
            ("repo_path".into(), "/data/acme/project/repo".to_string()),
        ]
        .into_iter()
        .collect();

        // Merge trigger-specific variables (simulating a PR trigger)
        let trigger_vars =
            std::collections::HashMap::from([("pr_number".to_string(), "7".to_string())]);
        for (k, v) in &trigger_vars {
            vars.insert(k.clone(), v.clone());
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
