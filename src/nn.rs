use std::path::PathBuf;

use tracing::instrument;

#[derive(Debug)]
pub struct NnPaths {
    config: PathBuf,
    tokenizer: PathBuf,
    model: PathBuf,
}

impl NnPaths {
    #[instrument(skip_all)]
    pub async fn get_all(api: &mut hf_hub::api::tokio::Api) -> Result<Self, Box<dyn std::error::Error>> {
        let repo = api.repo(hf_hub::Repo::with_revision(
            "openai/whisper-tiny".to_string(),
            hf_hub::RepoType::Model,
            "main".to_string(),
        ));
        let config = repo.get("config.json").await?;
        let tokenizer = repo.get("tokenizer.json").await?;
        let model = repo.get("model.safetensors").await?;

        Ok(Self {
            config,
            tokenizer,
            model,
        })
    }
}