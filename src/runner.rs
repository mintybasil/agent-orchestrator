use crate::hermes::invoke;
use crate::template::render;
use crate::workflow::{workflow, Validation};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Identifies a GitHub issue uniquely.
pub struct IssueKey {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl std::fmt::Display for IssueKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Run all workflow steps for a single issue.
/// Returns Ok(()) if all steps succeeded, Err if any step failed.
/// data_root is the base data/ directory (e.g. PathBuf::from("data")).
pub async fn run_issue(key: &IssueKey, data_root: &Path) -> Result<()> {
    let issue_dir = data_root
        .join(&key.owner)
        .join(&key.repo)
        .join(key.number.to_string());
    fs::create_dir_all(&issue_dir)?;

    let steps = workflow();
    for (idx, step) in steps.iter().enumerate() {
        let output_path = issue_dir.join(step.output_file);
        let error_path = issue_dir.join(format!("step_{:02}_{}.error", idx, step.name));

        // Build template variables: standard vars + paths to all prior step outputs.
        let mut var_pairs: Vec<(String, String)> = vec![
            ("owner".to_string(), key.owner.clone()),
            ("repo".to_string(), key.repo.clone()),
            ("issue_number".to_string(), key.number.to_string()),
            ("output_path".to_string(), output_path.to_string_lossy().into_owned()),
        ];
        for prior in 0..idx {
            let prior_path = issue_dir.join(steps[prior].output_file);
            var_pairs.push((
                format!("step_{}_output", prior),
                prior_path.to_string_lossy().into_owned(),
            ));
        }
        let vars: HashMap<&str, String> = var_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

        let prompt = render(step.prompt_template, &vars);

        tracing::info!("[{}] {}: started", key, step.name);

        // hermes::invoke is sync; run it in a blocking thread pool.
        let prompt_clone = prompt.clone();
        let error_path_clone = error_path.clone();
        let result = tokio::task::spawn_blocking(move || invoke(&prompt_clone, &error_path_clone))
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {}", e))??;

        // Validate the step's output according to its declared validation rule.
        match step.validation {
            Validation::FileNonEmpty => match fs::metadata(&output_path) {
                Ok(m) if m.len() > 0 => {}
                Ok(_) => {
                    let msg = "output file is empty";
                    let _ = fs::write(&error_path, msg);
                    tracing::error!("[{}] {}: FAILED ({})", key, step.name, msg);
                    anyhow::bail!("{}: {}", step.name, msg);
                }
                Err(e) => {
                    let msg = format!("output file missing: {}", e);
                    let _ = fs::write(&error_path, &msg);
                    tracing::error!("[{}] {}: FAILED ({})", key, step.name, msg);
                    anyhow::bail!("{}: {}", step.name, msg);
                }
            },
        }

        tracing::info!("[{}] {}: completed", key, step.name);
    }

    Ok(())
}
