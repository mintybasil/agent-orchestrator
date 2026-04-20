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
        vars.insert("output_path", "/data/repo/42/step_00_triage.md".to_string());
        let result = render("Write triage to {{output_path}}.", &vars);
        assert_eq!(result, "Write triage to /data/repo/42/step_00_triage.md.");
    }
}
