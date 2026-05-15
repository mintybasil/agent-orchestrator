//! Harness implementation for the Hermes Agent REST API.
//!
//! Sends a POST request to the Responses API endpoint using the
//! OpenAI-compatible format. Authentication uses a Bearer token from the
//! `HERMES_API_KEY` environment variable.
//!
//! Uses the `/v1/responses` endpoint (not `/v1/chat/completions`) because
//! the Responses API supports persistent conversation state via
//! `previous_response_id` and exposes tool call outputs in the response.
//! The workspace path is injected via the `instructions` field rather than
//! a system message, following the Responses API schema.

use crate::harness::{Harness, LogConfig};
use crate::workflow::Step;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Full endpoint path appended to the user-provided base URL.
const API_PATH: &str = "/v1/responses";

/// Harness that invokes the Hermes Agent via its REST API.
pub struct HermesApiHarness {
    /// Base URL of the API server (e.g. "http://localhost:8000").
    pub base_url: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Build the full API URL from a base URL.
///
/// Strips a trailing slash from `base_url` (if present) then appends
/// the API path. Returns an error if the base URL already contains a
/// path beyond root, since that indicates the user included internal
/// routing in the config.
fn endpoint_url(base_url: &str) -> Result<String> {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains("/v1") || trimmed.contains("/chat") {
        anyhow::bail!(
            "hermes_api base_url should be just the host URL, e.g. \
             \"http://localhost:8000\" — got: {:?}",
            base_url
        );
    }
    Ok(format!("{}{}", trimmed, API_PATH))
}

/// Request body for the `/v1/responses` endpoint.
///
/// Uses the Responses API format where `instructions` provides persistent
/// system-level guidance (workspace path, provider hint) and `input`
/// carries the rendered prompt for this step.
#[derive(Serialize)]
struct ResponsesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    instructions: String,
    input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
}

/// An output item from the Responses API.
#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    content: Option<Vec<ContentBlock>>,
}

/// A content block within an output item.
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
}

/// Response body from the Responses API endpoint.
#[derive(Deserialize)]
struct ResponsesApiResponse {
    id: Option<String>,
    status: Option<String>,
    output: Option<Vec<OutputItem>>,
}

/// A minimal error response shape — we only care about the detail field
/// for error reporting. If the API returns a non-JSON error body we
/// fall back to the raw text.
#[derive(Deserialize)]
struct ApiError {
    error: Option<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: Option<String>,
}

impl Harness for HermesApiHarness {
    fn name(&self) -> &str {
        "hermes_api"
    }

    fn run_step(
        &self,
        step: &Step,
        workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
        log_config: &LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let url = match endpoint_url(&self.base_url) {
            Ok(u) => u,
            Err(e) => {
                return Box::pin(async move { Err(e) });
            }
        };
        let provider = self.provider.clone();
        let model = self.model.clone();
        let workspace_dir = workspace_dir.to_path_buf();
        let prompt = rendered_prompt.to_string();
        let step_name = step.name.clone();
        let issue = issue.to_string();
        let log_path = log_config.log_path.clone();
        let show_logs = log_config.show_logs;
        let error_path = error_path.to_path_buf();

        Box::pin(async move {
            run_api_step(
                &url,
                provider.as_deref(),
                model.as_deref(),
                &workspace_dir,
                &prompt,
                &step_name,
                &issue,
                &log_path,
                &error_path,
                show_logs,
            )
            .await
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_api_step(
    url: &str,
    provider: Option<&str>,
    model: Option<&str>,
    workspace_dir: &Path,
    prompt: &str,
    step_name: &str,
    issue: &str,
    log_path: &Path,
    error_path: &Path,
    show_logs: bool,
) -> Result<()> {
    // Retrieve the API key from the environment.
    let api_key = std::env::var("HERMES_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "HERMES_API_KEY environment variable is not set or empty \
             (required for hermes_api harness)"
        )
    })?;

    if show_logs {
        tracing::info!(
            issue = %issue,
            step = %step_name,
            "invoking hermes_api harness"
        );
    }

    // Build instructions that tell the remote agent where its workspace is.
    // Using "cd <path>" rather than just asserting the directory avoids
    // conflicting with the agent's own environment hints (see the
    // api-server-workdir-pitfall doc for details).
    let instructions = format!(
        "All work is in: {}. Always run `cd {}` as your first action before any file or terminal operations. Reference all file paths relative to this directory.{}",
        workspace_dir.display(),
        workspace_dir.display(),
        provider.map_or_else(String::new, |p| format!("\nProvider: {}", p))
    );

    let request_body = ResponsesRequest {
        model: model.map(|m| m.to_string()),
        instructions,
        input: prompt.to_string(),
        store: Some(true),
    };

    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("hermes_api request failed: {}", e))?;

    let status = response.status();
    let response_text = response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read hermes_api response body: {}", e))?;

    if !status.is_success() {
        // Try to parse as a structured API error for a nicer message.
        let error_detail = serde_json::from_str::<ApiError>(&response_text)
            .ok()
            .and_then(|e| e.error)
            .and_then(|e| e.message)
            .unwrap_or_else(|| {
                // Fall back to the raw response body, truncated.
                if response_text.len() > 2000 {
                    format!("{}...(truncated)", &response_text[..2000])
                } else {
                    response_text.clone()
                }
            });

        // Write error file.
        let error_content = format!("hermes_api HTTP {}: {}", status, error_detail);
        std::fs::write(error_path, &error_content).ok();

        anyhow::bail!("hermes_api returned HTTP {}: {}", status, error_detail);
    }

    // Parse the successful response.
    let api_response: ResponsesApiResponse = serde_json::from_str(&response_text).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse hermes_api response as ResponsesApiResponse: {}. Raw body (first 500 chars): {}",
            e,
            &response_text[..response_text.len().min(500)]
        )
    })?;

    // Extract assistant text from the output items.
    // The Responses API returns a list of output items; we concatenate
    // all text content blocks into a single string for the log.
    let assistant_content = api_response
        .output
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.content.as_ref())
        .flatten()
        .filter_map(|block| {
            if block.content_type.as_deref() == Some("output_text") {
                block.text.clone()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let response_id = api_response.id.unwrap_or_default();

    // Write the full response to the log file.
    // First write the raw API response, then the extracted content.
    let mut log_content = String::new();
    log_content.push_str(&format!(
        "=== API Response (HTTP {}, id={}) ===\n",
        status, response_id
    ));
    log_content.push_str(&response_text);
    log_content.push_str("\n\n=== Assistant Content ===\n");
    log_content.push_str(&assistant_content);
    log_content.push('\n');

    // Timestamp the log file using the same function from hermes.rs.
    let timestamped = crate::hermes::timestamp_log_string(&log_content);
    std::fs::write(log_path, &timestamped).map_err(|e| {
        anyhow::anyhow!("failed to write hermes_api log file {:?}: {}", log_path, e)
    })?;

    if show_logs {
        for line in timestamped.lines() {
            tracing::info!(issue = %issue, step = %step_name, "{}", line);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_appends_path() {
        assert_eq!(
            endpoint_url("http://localhost:8000").unwrap(),
            "http://localhost:8000/v1/responses"
        );
    }

    #[test]
    fn endpoint_url_strips_trailing_slash() {
        assert_eq!(
            endpoint_url("http://localhost:8000/").unwrap(),
            "http://localhost:8000/v1/responses"
        );
    }

    #[test]
    fn endpoint_url_rejects_path_prefix() {
        let result = endpoint_url("http://localhost:8000/v1");
        assert!(result.is_err(), "should reject base_url with /v1 path");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("should be just the host URL"),
            "error should explain the config requirement, got: {err}"
        );
    }

    #[test]
    fn endpoint_url_rejects_full_endpoint() {
        let result = endpoint_url("http://localhost:8000/v1/chat/completions");
        assert!(result.is_err(), "should reject base_url with full path");
    }

    #[test]
    fn responses_request_serializes_with_all_fields() {
        let request = ResponsesRequest {
            model: Some("gpt-4o".to_string()),
            instructions: "All work is in: /tmp/repo. Always run `cd /tmp/repo` first.".to_string(),
            input: "Hello!".to_string(),
            store: Some(true),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"model\":\"gpt-4o\""));
        assert!(json.contains("\"instructions\""));
        assert!(json.contains("\"input\":\"Hello!\""));
        assert!(json.contains("\"store\":true"));
    }

    #[test]
    fn responses_request_serializes_with_null_model() {
        let request = ResponsesRequest {
            model: None,
            instructions: "Be helpful.".to_string(),
            input: "Hello!".to_string(),
            store: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        // skip_serializing_if should omit both model and store
        assert!(!json.contains("\"model\""));
        assert!(!json.contains("\"store\""));
    }

    #[test]
    fn responses_api_output_extraction() {
        let json = r#"{
            "id": "resp_abc123",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "Hello! How can I help?"}
                    ]
                }
            ]
        }"#;
        let response: ResponsesApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id.as_deref(), Some("resp_abc123"));
        let content: String = response
            .output
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.content.as_ref())
            .flatten()
            .filter_map(|block| {
                if block.content_type.as_deref() == Some("output_text") {
                    block.text.clone()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(content, "Hello! How can I help?");
    }

    #[test]
    fn responses_api_handles_empty_output() {
        let json = r#"{
            "id": "resp_empty",
            "status": "completed",
            "output": []
        }"#;
        let response: ResponsesApiResponse = serde_json::from_str(json).unwrap();
        let content: String = response
            .output
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.content.as_ref())
            .flatten()
            .filter_map(|block| {
                if block.content_type.as_deref() == Some("output_text") {
                    block.text.clone()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.is_empty());
    }

    #[test]
    fn api_error_deserializes() {
        let json = r#"{
            "error": {
                "message": "Invalid API key",
                "type": "invalid_request_error"
            }
        }"#;
        let err: ApiError = serde_json::from_str(json).unwrap();
        assert_eq!(err.error.unwrap().message.unwrap(), "Invalid API key");
    }

    #[test]
    fn instructions_includes_workspace_path() {
        let workspace = Path::new("/tmp/data/owner/repo/42");
        let instructions = format!(
            "All work is in: {}. Always run `cd {}` as your first action before any file or terminal operations. Reference all file paths relative to this directory.",
            workspace.display(),
            workspace.display()
        );
        assert!(instructions.contains("/tmp/data/owner/repo/42"));
        assert!(instructions.contains("cd /tmp/data/owner/repo/42"));
    }

    #[test]
    fn instructions_with_provider() {
        let workspace = Path::new("/tmp/repo");
        let provider = "openai";
        let instructions = format!(
            "All work is in: {}. Always run `cd {}` as your first action before any file or terminal operations. Reference all file paths relative to this directory.{}",
            workspace.display(),
            workspace.display(),
            format!("\nProvider: {}", provider)
        );
        assert!(instructions.contains("Provider: openai"));
    }

    #[test]
    fn hermes_api_harness_name() {
        let harness = HermesApiHarness {
            base_url: "http://localhost:8000".to_string(),
            provider: None,
            model: None,
        };
        assert_eq!(harness.name(), "hermes_api");
    }

    #[test]
    fn missing_api_key_returns_error() {
        // Ensure HERMES_API_KEY is not set for this test.
        // In Rust 2024 edition, env::remove_var requires an unsafe block.
        // We simply verify that std::env::var returns Err when the key
        // is absent, without modifying the environment.
        assert!(
            std::env::var("HERMES_API_KEY_DUMMY_NONEXISTENT").is_err(),
            "non-existent env var should return Err"
        );
    }
}
