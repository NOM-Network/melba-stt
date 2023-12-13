use std::sync::Arc;

use bot::stream::StreamProcessor;
use nn::model::ModelBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::prelude::*;

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var(
            "RUST_LOG",
            "info,bot=trace,recorder=trace,tokenizers::tokenizer::serialization=error",
        );
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(
            tracing_subscriber::fmt::layer()
                .with_file(true)
                .with_line_number(true),
        )
        .init();

    let config = tokio::fs::read_to_string("bot.toml").await?;
    let config: bot::config::SttConfig = toml::from_str(&config)?;
    let secrets = tokio::fs::read_to_string("secrets.toml").await?;
    let secrets: bot::config::Secrets = toml::from_str(&secrets)?;

    let model = ModelBuilder::default()
        .cuda_or_cpu(0)
        .finish()
        .await
        .unwrap();

    let stream_processor = Arc::new(StreamProcessor::new(model));

    let recorder =
        recorder::Recorder::new(format!("recordings/{}", config.channel_to_join.guild_id).into())
            .await;

    let mut client = bot::discord::setup_discord_bot(
        &secrets.discord_token,
        stream_processor,
        recorder,
        config.clone(),
    )
    .await;
    client.start().await.expect("run failed");

    Ok(())
}
