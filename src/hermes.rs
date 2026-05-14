    fn run_step(
        &self,
        _step: &Step,
        _workspace_dir: &Path,
        rendered_prompt: &str,
        error_path: &Path,
        issue: &str,
        log_config: &crate::harness::LogConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>> {
        let args = InvokeApiArgs {
            base_url: &self.base_url,
            api_key: &self.api_key,
            model: &self.model,
            prompt: rendered_prompt,
            error_file: error_path,
            log_file: &log_config.log_path,
            show_logs: log_config.show_logs,
            issue,
        };

        Box::pin(async move {
            tokio::task::spawn_blocking(move || invoke_api(&args))
                .await?
        })
    }
}