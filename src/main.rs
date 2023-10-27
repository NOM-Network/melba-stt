use std::sync::Arc;

use melba_stt::{
    audio::handle_audio_streams,
    config,
    discord::{setup_discord_bot, CompletedMessage},
    nn::{self},
};

use tracing::{debug, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::prelude::*;

    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var(
            "RUST_LOG",
            "info,rustls=info,ureq=info,tokenizers::tokenizer::serialization=error,serenity=info,tungstenite=info,songbird=info,hyper=info",
        ); //fixme: a bunch of warnings from `tokenizers`
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = tokio::fs::read_to_string("bot.toml").await?;
    let config = toml::from_str::<config::SttConfig>(&config)?;
    let config = Arc::new(config);
    let secrets = tokio::fs::read_to_string("secrets.toml").await?;
    let secrets = toml::from_str::<config::Secrets>(&secrets)?;

    let device = candle_core::Device::cuda_if_available(0).expect("failed to find device");
    info!(?device, "using");
    let model = nn::get_model(device).await?;

    let (mut client, _, channel_init_recv) =
        setup_discord_bot(secrets.discord_token, config.clone()).await?;

    let (completed_messages_send, mut completed_messages_recv) =
        tokio::sync::mpsc::channel::<CompletedMessage>(16);

    let handle_streams_task = tokio::task::spawn(async move {
        handle_audio_streams(model, channel_init_recv, completed_messages_send).await
    });

    let debug_print_task = tokio::task::spawn(async move {
        while let Some(msg) = completed_messages_recv.recv().await {
            // info!(who=?msg.who, content=msg.message, "completed message");
            let min_no_speech_prob = msg
                .message
                .iter()
                .map(|segment| (segment.dr.no_speech_prob * 1_000_000.0) as i32)
                .min()
                .unwrap_or(1_000_000) as f32
                / 1_000_000.0;

            let contents = msg
                .message
                .iter()
                .map(|segment| segment.dr.text.to_owned())
                .collect::<Vec<_>>();

            debug!(min_no_speech_prob, "converted message");
            if min_no_speech_prob < 0.45 {
                info!(?contents, ?msg.who, "completed message");
            } else {
                debug!(?contents, ?msg.who, "message had high no speech probability")
            }
        }
    });

    let client_fut = client.start();
    info!("starting client");
    tokio::select! {
        t = client_fut => t.unwrap(),
        s = handle_streams_task => s.unwrap(),
        p = debug_print_task => p.unwrap(),
    }
    info!("done");

    Ok(())
}
