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

/// Invoke hermes via the HTTP API server.
///
/// Sends a POST request to `{base_url}/chat/completions` with the prompt
/// and captures the response. All output is written to `log_file`.
///
/// Returns `Ok(())` on successful completion.
/// On error: writes error details to `error_file` and returns `Err`.
fn invoke_api(
    base_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    error_file: &Path,
    log_file: &PathBuf,
    show_logs: bool,
    issue: &str,
) -> Result<()> {
    use reqwest::blocking::Client;
    use serde_json::json;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {}", e))?;

    let base_url = base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    let body = json!({
        "model": model,
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "stream": false
    });

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

    let status = response.status();
    let response_text = response
        .text()
        .map_err(|e| anyhow::anyhow!("failed to read response: {}", e))?;

    let file = std::fs::File::create(log_file)
        .map_err(|e| anyhow::anyhow!("failed to create log file {:?}: {}", log_file, e))?;
    let mut writer = std::io::BufWriter::new(file);

    let timestamped_response = timestamp_line(&format!(
        "API Response (status: {}):\n{}",
        status, response_text
    ));
    writeln!(writer, "{}", timestamped_response)
        .map_err(|e| anyhow::anyhow!("failed to write to log file: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if show_logs {
        tracing::info!(issue = %issue, "API response status: {}", status);
        tracing::info!(issue = %issue, "API response: {}", response_text);
    }

    if !status.is_success() {
        let error_content = format!("HTTP Error: {}\n\nResponse:\n{}", status, response_text);
        if let Err(e) = std::fs::write(error_file, &error_content) {
            tracing::warn!("failed to write error file {:?}: {}", error_file, e);
        }
        anyhow::bail!("API request failed with status {}", status);
    }

    let response_json: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| anyhow::anyhow!("failed to parse response JSON: {}", e))?;

    let assistant_message = response_json
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("<no content in response>");

    let message_log = timestamp_line(&format!("Assistant: {}", assistant_message));
    writeln!(writer, "{}", message_log)
        .map_err(|e| anyhow::anyhow!("failed to write message to log: {}", e))?;
    writer
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to flush log file: {}", e))?;

    if show_logs {
        tracing::info!(issue = %issue, "Assistant response: {}", assistant_message);
    }

    Ok(())
}
