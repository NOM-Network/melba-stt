use std::sync::Arc;

use actual_discord_stt::{nn::NnPaths, discord::setup_discord_bot, audio::handle_audio_streams};

use futures::TryFutureExt;
use serenity::async_trait;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, instrument, trace, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::prelude::*;

    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var(
            "RUST_LOG",
            "trace,rustls=info,ureq=info,tokenizers::tokenizer::serialization=error,serenity=info,tungstenite=info,songbird=info,hyper=info",
        ); //fixme: a bunch of warnings from `tokenizers`
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let mut hf_api = hf_hub::api::tokio::ApiBuilder::new()
        .with_token(None)
        .with_progress(true)
        .build()?;
    let nn_paths = NnPaths::get_all(&mut hf_api).await?;

    info!(?nn_paths, "got paths");

    let (mut client, r, channel_init_recv) = setup_discord_bot().await?;

    let handle_streams_task =
        tokio::task::spawn(async move { handle_audio_streams(r, channel_init_recv).await });

    let timeout_fut = tokio::time::timeout(std::time::Duration::from_secs(30), client.start());
    info!("starting client");
    tokio::select! {
        _ = timeout_fut => (),
        _ = handle_streams_task => (),
    }
    info!("done");

    Ok(())
}