// Template placeholders:
// {{owner}}          - repository owner
// {{repo}}           - repository name
// {{issue_number}}   - GitHub issue number
// {{output_path}}    - full path to this step's output file
// {{step_0_output}}  - full path to step 0's output file (pattern: step_N_output for any prior step)

/// How to validate a step's output after hermes exits successfully.
#[derive(Debug, Clone)]
pub enum Validation {
    /// The output file must exist and be non-empty.
    FileNonEmpty,
}

/// A single step in the agent workflow.
#[derive(Debug, Clone)]
pub struct Step {
    pub name: &'static str,
    pub prompt_template: &'static str,
    pub output_file: &'static str,
    pub validation: Validation,
}

/// Returns the hardcoded workflow.
pub fn workflow() -> Vec<Step> {
    vec![
        Step {
            name: "triage",
            prompt_template: "Read GitHub issue #{{issue_number}} in {{owner}}/{{repo}}. \
                              Write a triage summary to {{output_path}}.",
            output_file: "step_00_triage.md",
            validation: Validation::FileNonEmpty,
        },
        Step {
            name: "implement",
            prompt_template: "Read the triage at {{step_0_output}}. \
                              Implement the changes described. \
                              Write a summary of what you did to {{output_path}}.",
            output_file: "step_01_implement.md",
            validation: Validation::FileNonEmpty,
        },
    ]
}
