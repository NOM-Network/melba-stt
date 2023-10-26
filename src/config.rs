use serde::Deserialize;

type Snowflake = u64;

#[derive(Deserialize)]
pub struct Secrets {
    pub discord_token: String,
}

#[derive(Debug, Deserialize)]
pub struct SttConfig {
    pub channel_to_join: Channel,
    #[serde(default)]
    pub ignored_users: Vec<Snowflake>,
    #[serde(default)]
    pub model: ModelSpec,
}

#[derive(Debug, Deserialize)]
pub struct Channel {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
}

#[derive(Debug, Deserialize, Default)]
pub struct ModelSpec {
    #[serde(default)]
    pub repo: RepoSpec,
    #[serde(default)]
    pub filenames: FilesSpec,
}

#[derive(Debug, Deserialize)]
pub struct RepoSpec {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub revision: String,
}

impl Default for RepoSpec {
    fn default() -> Self {
        Self {
            repo: "openai/whisper-tiny".to_string(),
            revision: "main".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FilesSpec {
    #[serde(default)]
    pub config: String,
    #[serde(default)]
    pub tokenizer: String,
    #[serde(default)]
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
