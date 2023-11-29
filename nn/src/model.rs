use thiserror::Error;

use crate::whisper::Decoder;

// use crate::whisper::{SharedAudio, SharedText, SharedWhisperParts};

#[derive(Debug, Error)]
pub enum Error {
    #[error("error from hf_hub client: {0}")]
    HfHub(#[from] hf_hub::api::tokio::ApiError),
    #[error("missing field `{0}`")]
    MissingField(String),
    #[error("io operation failed {0:?}")]
    IoError(#[from] std::io::Error),
    #[error("json failed {0:?}")]
    Json(#[from] serde_json::Error),
    #[error("tokenizers error {0:?}")]
    Tokenizers(#[from] tokenizers::Error),
    #[error("candle errir {0:?}")]
    Candle(#[from] candle_core::Error),
}

pub struct ModelBuilder {
    repo: Option<hf_hub::Repo>,
    file_spec: Option<FilesSpec>,
    device: Option<candle_core::Device>,
}

impl Default for ModelBuilder {
    fn default() -> Self {
        Self {
            repo: Some(hf_hub::Repo::with_revision(
                "openai/whisper-tiny".to_string(),
                hf_hub::RepoType::Model,
                "main".to_string(),
            )),
            file_spec: Some(FilesSpec::default()),
            device: None,
        }
    }
}

impl ModelBuilder {
    pub fn new() -> Self {
        Self {
            repo: None,
            file_spec: None,
            device: None,
        }
    }

    pub fn set_repo(mut self, repo: hf_hub::Repo) -> Self {
        self.repo = Some(repo);
        self
    }

    pub fn set_file_spec(mut self, spec: FilesSpec) -> Self {
        self.file_spec = Some(spec);
        self
    }

    pub fn set_device(mut self, device: candle_core::Device) -> Self {
        self.device = Some(device);
        self
    }

    pub fn cuda_or_cpu(mut self, index: usize) -> Self {
        self.device = Some(candle_core::Device::cuda_if_available(index).unwrap());
        self
    }

    pub async fn finish<'a>(self) -> Result<ModelContainer, Error> {
        let api = hf_hub::api::tokio::Api::new()?;
        let repo = api.repo(self.repo.ok_or(Error::MissingField("repo".to_string()))?);

        let FilesSpec {
            config,
            tokenizer,
            model,
        } = self
            .file_spec
            .ok_or(Error::MissingField("file_spec".to_string()))?;

        let device = self
            .device
            .ok_or(Error::MissingField("device".to_string()))?;

        let config = repo.get(&config).await?;
        let tokenizer = repo.get(&tokenizer).await?;
        let model = repo.get(&model).await?;

        let config = tokio::fs::read_to_string(config).await?;
        let config: candle_transformers::models::whisper::Config = serde_json::from_str(&config)?;
        let tokenizer = tokio::fs::read(tokenizer).await?;
        let tokenizer = tokenizers::Tokenizer::from_bytes(tokenizer)?;
        let model = tokio::fs::read(model).await?;
        let model = candle_nn::VarBuilder::from_buffered_safetensors(
            model,
            candle_transformers::models::whisper::DTYPE,
            &device,
        )?;

        let whisper = candle_transformers::models::whisper::model::Whisper::load(&model, config.clone())?;

        Ok(ModelContainer {
            model_data: ModelData {
                config,
                tokenizer,
                // model,
            },
            device,
            whisper,
        })
    }
}

pub struct ModelContainer {
    model_data: ModelData,
    device: candle_core::Device,
    whisper: candle_transformers::models::whisper::model::Whisper,
    // shared_parts: Arc<SharedWhisperParts>,
}

impl ModelContainer {
    pub fn get_new_speaker(&self) -> SpeakerProcessor {
        let tokenizer = self.model_data.tokenizer.clone();
        let lt = tokenizer.token_to_id("<|en|>");
        let decoder = Decoder::new(
            self.whisper.clone(),
            self.device.clone(),
            tokenizer,
            0,
            lt,
        ).unwrap();

        SpeakerProcessor {
            decoder: decoder,
        }
        // // todo: contruct a whisper model somewhere
        // e.g. this would load a model
        // but every time `Whisper::load` is called it allocates a bunch of gpu memory

        // SpeakerProcessor {
        //     model: SharedWhisperParts,
        // }
    }
}

pub struct ModelData {
    config: candle_transformers::models::whisper::Config,
    tokenizer: tokenizers::Tokenizer,
    // model: candle_nn::var_builder::VarBuilderArgs<'a, Box<dyn candle_nn::var_builder::SimpleBackend>>,
}

pub struct SpeakerProcessor {
    pub decoder: Decoder,
}

// pub struct SharedModel {}

#[derive(Debug)]
pub struct FilesSpec {
    pub config: String,
    pub tokenizer: String,
    pub model: String,
}

impl Default for FilesSpec {
    fn default() -> Self {
        Self {
            config: "config.json".to_string(),
            tokenizer: "tokenizer.json".to_string(),
            model: "model.safetensors".to_string(),
        }
    }
}