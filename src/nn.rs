pub mod model;
pub mod w_reimpl;

use std::{path::PathBuf, sync::Arc};

use candle_transformers::models::whisper;
use tracing::{debug, instrument};

pub async fn get_model(
    device: candle_core::Device,
) -> Result<ModelContainer, Box<dyn std::error::Error>> {
    let mut hf_api = hf_hub::api::tokio::ApiBuilder::new()
        .with_progress(true)
        .build()?;

    let nn_paths = NnPaths::get_all(&mut hf_api).await?;
    debug!(?nn_paths, "got paths");
    let tokenizer = load_tokenizer(nn_paths.tokenizer)?;
    let config = load_config(nn_paths.config)?;
    let mel_filters = load_mel_filters(include_bytes!("melfilters.bytes"));
    let varbuilder = load_model_file_to_varbuilder(nn_paths.model, &device)?;

    let lang_token = tokenizer.token_to_id("<|en|>");
    let m = w_reimpl::Whisper::load(&varbuilder, config)?;
    let d = model::Decoder::new(m, device.clone(), tokenizer, 0, lang_token)?;

    Ok(ModelContainer::new(d, device.clone(), mel_filters))
}

fn load_tokenizer(
    tokenizer_path: PathBuf,
) -> Result<tokenizers::Tokenizer, Box<dyn std::error::Error>> {
    Ok(tokenizers::Tokenizer::from_file(tokenizer_path).unwrap()) // fixme: this can probably be easily fixed by mapping to our own error type
}

fn load_config(config_path: PathBuf) -> Result<whisper::Config, Box<dyn std::error::Error>> {
    let config_contents = std::fs::read_to_string(config_path)?;
    serde_json::from_str(&config_contents).map_err(|e| e.into())
}

fn load_mel_filters(filter_bytes: &[u8]) -> Vec<f32> {
    filter_bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<f32>>()
}

fn load_model_file_to_varbuilder(
    model_path: PathBuf,
    device: &candle_core::Device,
) -> Result<candle_nn::VarBuilder, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(model_path)?;
    let varbuilder =
        candle_nn::VarBuilder::from_buffered_safetensors(bytes, whisper::DTYPE, &device)?;
    Ok(varbuilder)
}

#[derive(Debug)]
pub struct NnPaths {
    config: PathBuf,
    tokenizer: PathBuf,
    model: PathBuf,
}

impl NnPaths {
    #[instrument(skip_all)]
    pub async fn get_all(
        api: &mut hf_hub::api::tokio::Api,
    ) -> Result<Self, Box<dyn std::error::Error>> {
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

#[derive(Clone)]
pub struct ModelContainer {
    inner: Arc<model::Decoder>,
    device: candle_core::Device,
    mel_filters: Vec<f32>,
}

impl ModelContainer {
    pub fn new(inner: model::Decoder, device: candle_core::Device, mel_filters: Vec<f32>) -> Self {
        Self {
            inner: Arc::new(inner),
            device,
            mel_filters,
        }
    }

    pub fn predict(&self, data: Vec<f32>) -> Vec<model::Segment> {
        let mel = whisper::audio::pcm_to_mel(&data, &self.mel_filters);
        let mel_len = mel.len();
        let mel = candle_core::Tensor::from_vec(
            mel,
            (1, whisper::N_MELS, mel_len / whisper::N_MELS),
            &self.device,
        )
        .unwrap();
        // let mut model = self.inner.lock().unwrap();
        self.inner.run(&mel).unwrap()
    }
}
