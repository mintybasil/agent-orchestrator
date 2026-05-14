/// Harness implementation for the hermes API server.
///
/// Uses the OpenAI-compatible `/v1/chat/completions` endpoint to invoke hermes.
/// The API server captures all agent output (including tool calls) in the HTTP response.
pub struct HermesApiHarness {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Harness for HermesApiHarness {
    fn name(&self) -> &str {
        "hermes-api"
    }

    fn run_step(
        &self,
        _step: &Step,
        _workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
        log_config: &crate::harness::LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let prompt = rendered_prompt.to_string();
        let error_file = error_path.to_path_buf();
        let log_file = log_config.log_path.clone();
        let show_logs = log_config.show_logs;
        let issue_str = issue.to_string();

        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                invoke_api(
                    &base_url,
                    &api_key,
                    &model,
                    &prompt,
                    &error_file,
                    &log_file,
                    show_logs,
                    &issue_str,
                )
            })
            .await?
        })
    }
}

/// Parameters for [`invoke_api`].
///
/// Grouped into a struct to avoid too many arguments in the function signature.
struct InvokeApiArgs<'a> {
    base_url: &'a str,
    api_key: &'a str,
    model: &'a str,
    prompt: &'a str,
    error_file: &'a Path,
    log_file: &'a PathBuf,
    show_logs: bool,
    issue: &'a str,
}

/// Invoke hermes via the HTTP API server.
///
/// Sends a POST request to `{base_url}/chat/completions` with the prompt
/// and captures the response. All output is written to `log_file`.
///
/// Returns `Ok(())` on successful completion.
/// On error: writes error details to `error_file` and returns `Err`.
fn invoke_api(args: &InvokeApiArgs<'_>) -> Result<()> {
    use reqwest::blocking::Client;
    use serde_json::json;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {}", e))?;

    // Ensure base_url doesn't have trailing slash for consistent URL construction
    let base_url = args.base_url.trim_end_matches('/');

    // Build the API endpoint URL
    // The API server supports both /v1/chat/completions and /chat/completions
    // We try /chat/completions first (more common pattern)
    let url = format!("{}/chat/completions", base_url);

    // Build the request body matching OpenAI chat completions format
    let body = json!({
        "model": args.model,
        "messages": [
            {"role": "user", "content": args.prompt}
        ],
        "stream": false
    });

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", args.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

    let status = response.status();
    let response_text = response
        .text()
        .map_err(|e| anyhow::anyhow!("failed to read response: {}", e))?;

    // Create log file and writer
    let file = std::fs::File::create(args.log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", args.log_file, e))?;
    let mut writer = std::io::BufWriter::new(file);

    // Write response to log file with timestamp
    let timestamped_response = timestamp_line(&format!(
        "API Response (status: {}):\n{}",
        status, response_text
    ));
    writeln!(writer, "{}", timestamped_response)
        .map_err(|e| anyhow::anyhow!("failed to write to log file: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if args.show_logs {
        tracing::info!(issue = %args.issue, "API response status: {}", status);
        tracing::info!(issue = %args.issue, "API response: {}", response_text);
    }

    // Check if the request was successful
    if !status.is_success() {
        // Write error details to error file
        let error_content = format!("HTTP Error: {}\n\nResponse:\n{}", status, response_text);
        if let Err(e) = std::fs::write(args.error_file, &error_content) {
            tracing::warn!("failed to write error file {:?}: {}", args.error_file, e);
        }
        anyhow::bail!("API request failed with status {}", status);
    }

    // Parse the response to extract the assistant's message
    let response_json: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| anyhow::anyhow!("failed to parse response JSON: {}", e))?;

    // Extract the assistant's response content
    let assistant_message = response_json
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("<no content in response>");

    // Log the extracted message
    let message_log = timestamp_line(&format!("Assistant: {}", assistant_message));
    writeln!(writer, "{}", message_log)
        .map_err(|e| anyhow::anyhow!("failed to write message to log: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if args.show_logs {
        tracing::info!(issue = %args.issue, "Assistant response: {}", assistant_message);
    }

    Ok(())
}
