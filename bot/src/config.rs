use serde::Deserialize;

type Snowflake = u64;

#[derive(Deserialize)]
pub struct Secrets {
    pub discord_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SttConfig {
    pub channel_to_join: Channel,
    pub ws_url: String,
    #[serde(default)]
    pub ignored_users: Vec<Snowflake>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Channel {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
}

// todo: re-add model config options?
