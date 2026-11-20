mod client;

pub use client::LMStudioClient;
use codex_core::config::Config;

/// Default OSS model to use when `--oss` is passed without an explicit `-m`.
pub const DEFAULT_OSS_MODEL: &str = "openai/gpt-oss-20b";

/// Checks that the requested model is already available in LM Studio.
pub async fn ensure_oss_ready(config: &Config) -> std::io::Result<()> {
    let model = config.model.as_deref().unwrap_or(DEFAULT_OSS_MODEL);
    let lmstudio_client = LMStudioClient::try_from_provider(config).await?;
    let models = lmstudio_client.fetch_models().await?;
    if !models.iter().any(|candidate| candidate == model) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Model {model} is not installed locally in LM Studio"),
        ));
    }

    // Load the model in the background
    tokio::spawn({
        let client = lmstudio_client.clone();
        let model = model.to_string();
        async move {
            if let Err(e) = client.load_model(&model).await {
                tracing::warn!("Failed to load model {}: {}", model, e);
            }
        }
    });

    Ok(())
}
