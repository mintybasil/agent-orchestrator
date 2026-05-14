            HarnessConfig::HermesApi {
                base_url,
                api_key,
                model,
            } => Box::new(crate::hermes::HermesApiHarness {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                model: model.clone().unwrap_or_else(|| "hermes-agent".to_string()),
            }),