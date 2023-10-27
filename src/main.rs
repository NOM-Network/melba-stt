use std::sync::Arc;

use melba_stt::{
    audio::handle_audio_streams,
    config,
    discord::{setup_discord_bot, CompletedMessage},
    nn::{self},
    ws::websocket_task,
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

    let (mut client, _, channel_init_recv, speaking_event_recv) =
        setup_discord_bot(secrets.discord_token, config.clone()).await?;

    let cache = client.cache_and_http.clone().cache.clone();

    let (completed_messages_send, completed_messages_recv) =
        tokio::sync::broadcast::channel::<CompletedMessage>(16);

    let handle_streams_task = tokio::task::spawn(async move {
        handle_audio_streams(model, channel_init_recv, completed_messages_send).await
    });

    let websocket_task = {
        let completed_messages_recv = completed_messages_recv.resubscribe();
        let cache = cache.clone();
        let config = config.clone();
        tokio::task::spawn(async move {
            websocket_task(completed_messages_recv, speaking_event_recv, cache, config).await;
        })
    };

    let debug_print_task = {
        let completed_messages_recv = completed_messages_recv.resubscribe();
        let cache = cache.clone();
        let config = config.clone();
        tokio::task::spawn(
            async move { tracing_task(completed_messages_recv, cache, config).await },
        )
    };

    let client_fut = client.start();
    info!("starting client");
    tokio::select! {
        t = client_fut => t.unwrap(),
        w = websocket_task => w.unwrap(),
        s = handle_streams_task => s.unwrap(),
        p = debug_print_task => p.unwrap(),
    }
    info!("done");

    Ok(())
}

async fn tracing_task(
    mut completed_messages_recv: tokio::sync::broadcast::Receiver<CompletedMessage>,
    cache: Arc<serenity::cache::Cache>,
    config: Arc<config::SttConfig>,
) {
    while let Ok(msg) = completed_messages_recv.recv().await {
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

        let who = cache
            .member(config.channel_to_join.guild_id, msg.who.user_id)
            .map(|member| member.user.name.clone())
            .unwrap_or_else(|| "did not find user".to_string());

        debug!(min_no_speech_prob, "converted message");
        if min_no_speech_prob < 0.45 {
            info!(?contents, ?who, who.id=msg.who.user_id, "completed message");
        } else {
            debug!(?contents, ?who, who.id=msg.who.user_id, "message had high no speech probability")
        }
    }
}
