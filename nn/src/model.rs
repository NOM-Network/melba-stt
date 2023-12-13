use thiserror::Error;

use crate::whisper::{self, Decoder};

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

    fn mel_filters(size: usize) -> Vec<f32> {
        let filter_bytes = match size {
            80 => include_bytes!("melfilters.bytes"),
            128 => unimplemented!("no support for whisper v3 yet"),
            filter_size => panic!("invalid filter size `{filter_size}`"),
        };

        filter_bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<f32>>()
    }

    pub async fn finish(self) -> Result<ModelContainer, Error> {
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
        let filters = Self::mel_filters(80);

        let whisper =
            candle_transformers::models::whisper::model::Whisper::load(&model, config.clone())?;

        Ok(ModelContainer {
            model_data: ModelData {
                config,
                tokenizer,
                filters,
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
}

impl ModelContainer {
    pub fn get_new_speaker(&self) -> SpeakerProcessor {
        let tokenizer = self.model_data.tokenizer.clone();
        let lt = tokenizer.token_to_id("<|en|>");
        let decoder = Decoder::new(
            self.whisper.clone(),
            self.device.clone(),
            tokenizer,
            rand::random(),
            lt,
        )
        .unwrap();

        SpeakerProcessor {
            decoder,
            config: self.model_data.config.clone(),
            filters: self.model_data.filters.clone(),
            device: self.device.clone(),
        }
    }
}

pub struct ModelData {
    config: candle_transformers::models::whisper::Config,
    tokenizer: tokenizers::Tokenizer,
    filters: Vec<f32>,
}

pub struct SpeakerProcessor {
    pub config: candle_transformers::models::whisper::Config,
    pub filters: Vec<f32>,
    pub device: candle_core::Device,
    pub decoder: Decoder,
}

impl SpeakerProcessor {
    pub fn predict(&mut self, data: Vec<f32>) -> Vec<whisper::Segment> {
        let mel = candle_transformers::models::whisper::audio::pcm_to_mel(
            &self.config,
            &data,
            &self.filters,
        );
        let mel_len = mel.len();
        let mel = candle_core::Tensor::from_vec(
            mel,
            (
                1,
                80, // todo: this is only valid for whisper v1, v2 and will break whsisper v3
                mel_len / 80,
            ),
            &self.device,
        )
        .unwrap();
        self.decoder.run(&mel).unwrap()
    }

    pub fn predict_with_mel(&mut self, data: Vec<Vec<f32>>) -> Vec<whisper::Segment> {
        let mut mel = vec![vec![0.0; data.len()]; data[0].len()];

        for i in 0..data.len() {
            for j in 0..data[0].len() {
                mel[j][i] = data[i][j];
            }
        }

        let mel = mel.into_iter().flatten().collect::<Vec<_>>();

        let mel_len = mel.len();
        let mel = candle_core::Tensor::from_vec(
            mel,
            (
                1,
                80, // todo: this is only valid for whisper v1, v2 and will break whsiper v3
                mel_len / 80,
            ),
            &self.device,
        )
        .unwrap();
        dbg!(mel.shape());
        self.decoder.run(&mel).unwrap()
    }
}

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
