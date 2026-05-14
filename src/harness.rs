    /// Invoke hermes via the HTTP API server.
    ///
    /// ```toml
    /// harness = { type = "hermes_api", base_url = "http://localhost:8642/v1", api_key = "change-me-local-dev" }
    /// ```
    HermesApi {
        /// Required: Base URL of the hermes API server (e.g., "http://localhost:8642/v1").
        base_url: String,
        /// Required: Bearer token for API authentication.
        api_key: String,
        /// Optional: Model name to use (defaults to "hermes-agent").
        #[serde(default)]
        model: Option<String>,
    },