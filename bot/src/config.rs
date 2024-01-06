use serde::Deserialize;

type Snowflake = u64;

#[derive(Deserialize)]
pub struct Secrets {
    pub discord_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub stt: Stt,
    pub recorder: Recorder,
    pub ws: Websocket,
}

#[derive(Debug, Deserialize, Clone)]

pub struct Stt {
    pub channel_to_join: Channel,
    #[serde(default)]
    pub ignored_users: Vec<Snowflake>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Websocket {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Channel {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Recorder {
    #[serde(default)]
    pub directory: std::path::PathBuf,
    #[serde(default)]
    pub ignored_users: Vec<Snowflake>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            directory: "./recordings/".into(),
            ignored_users: Default::default(),
        }
    }
}

// todo: re-add model config options?
