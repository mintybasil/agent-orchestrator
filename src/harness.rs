    #[test]
    fn hermes_api_config_deserializes() {
        let toml = r#"
type = "hermes_api"
base_url = "http://localhost:8642/v1"
api_key = "test-key"
model = "hermes-agent"
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::HermesApi {
                base_url,
                api_key,
                model,
            } => {
                assert_eq!(base_url, "http://localhost:8642/v1");
                assert_eq!(api_key, "test-key");
                assert_eq!(model, Some("hermes-agent".to_string()));
            }
        }
    }

    #[test]
    fn hermes_api_config_minimal() {
        let toml = r#"
type = "hermes_api"
base_url = "http://localhost:8642/v1"
api_key = "test-key"
"#;
        let config: HarnessConfig = toml::from_str(toml).unwrap();
        match config {
            HarnessConfig::HermesApi {
                base_url,
                api_key,
                model,
            } => {
                assert_eq!(base_url, "http://localhost:8642/v1");
                assert_eq!(api_key, "test-key");
                assert!(model.is_none());
            }
        }
    }

    #[test]
    fn build_hermes_api() {
        let config = HarnessConfig::HermesApi {
            base_url: "http://localhost:8642/v1".to_string(),
            api_key: "test-key".to_string(),
            model: None,
        };
        let harness = config.build();
        assert_eq!(harness.name(), "hermes-api");
    }
}
