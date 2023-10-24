use std::sync::Arc;

use actual_discord_stt::NnPaths;

use serenity::async_trait;
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

    let (mut client, r) = setup_discord_bot().await?;

    info!("starting client");
    let x = tokio::time::timeout(std::time::Duration::from_secs(30), client.start()).await;
    info!("done");

    info!(?r.voice_ssrc_map, ":3");
    let _ = x??;

    Ok(())
}

async fn setup_discord_bot() -> Result<(serenity::Client, Reciever), Box<dyn std::error::Error>> {
    use serenity::prelude::*;
    use songbird::SerenityInit;
    let reciever = Reciever::new();

    let songbird_config =
        songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode);

    let client = Client::builder(
        include_str!("token.secret"),
        GatewayIntents::non_privileged(),
    )
    .event_handler(Handler {
        reciever: reciever.clone(),
    })
    .register_songbird_from_config(songbird_config)
    .await?;

    Ok((client, reciever))
}

struct Handler {
    reciever: Reciever,
}

#[async_trait]
impl serenity::client::EventHandler for Handler {
    async fn ready(&self, ctx: serenity::client::Context, ready: serenity::model::gateway::Ready) {
        info!(event=?ready, "ready");
        let manager = songbird::get(&ctx).await.expect("songbird failed").clone();
        let (handler_lock, connect_result) =
            manager.join(647850202430046238, 647850202975174690).await;

        if let Err(error) = connect_result {
            error!(?error, "failed to join voice channel");
            panic!()
        }

        let mut handler = handler_lock.lock().await;
        handler.add_global_event(songbird::CoreEvent::VoicePacket.into(), self.reciever.clone());
        handler.add_global_event(songbird::CoreEvent::SpeakingUpdate.into(), self.reciever.clone());
        handler.add_global_event(songbird::CoreEvent::ClientDisconnect.into(), self.reciever.clone());
        handler.add_global_event(
            songbird::CoreEvent::SpeakingStateUpdate.into(),
            self.reciever.clone(),
        );
    }
}

#[derive(Clone)]
struct Reciever {
    voice_ssrc_map: Arc<dashmap::DashMap<u32, u64>>,
    inverse_ssrc_map: Arc<dashmap::DashMap<u64, u32>>,
}

impl Reciever {
    fn new() -> Self {
        Self {
            voice_ssrc_map: Arc::new(dashmap::DashMap::new()),
            inverse_ssrc_map: Arc::new(dashmap::DashMap::new()),
        }
    }

    fn add_user_with_ssrc(&self, ssrc: u32, user_id: u64) {
        self.voice_ssrc_map.insert(ssrc, user_id);
        self.inverse_ssrc_map.insert(user_id, ssrc);
    }

    fn get_user_by_ssrc(&self, ssrc: u32) -> Option<u64> {
        self.voice_ssrc_map.get(&ssrc).map(|kv| *kv.value())
    }

    fn get_user_by_id(&self, user_id: u64) -> Option<u32> {
        self.inverse_ssrc_map.get(&user_id).map(|kv| *kv.value())
    }

    fn remove_user_by_id(&self, user_id: u64) -> Option<u32> {
        if let Some(ssrc) = self.inverse_ssrc_map.get(&user_id) {
            let ssrc = ssrc.value();
            self.voice_ssrc_map.remove(ssrc);
            return Some(*ssrc);
        }
        None
    }

    fn remove_user_by_ssrc(&self, ssrc: u32) -> Option<u64> {
        if let Some(ssrc) = self.voice_ssrc_map.get(&ssrc) {
            let user_id = ssrc.value();
            self.inverse_ssrc_map.remove(user_id);
            return Some(*user_id);
        }
        None
    }
}

#[async_trait]
impl songbird::EventHandler for Reciever {
    #[instrument(skip_all)]
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match ctx {
            songbird::EventContext::SpeakingStateUpdate(event) => {
                trace!(?event, "speaking state update");
                self.add_user_with_ssrc(event.ssrc, event.user_id.unwrap().0);
            }
            songbird::EventContext::VoicePacket(pckt) => {
                if let Some(pcm) = pckt.audio {
                    // pckt.packet.ssrc
                    if pcm.len() == 0 {
                        debug!(?pckt, "got zero length audio packet");
                    }
                    let silent = pcm.iter().filter(|item| **item != 0i16).count() == 0;
                    let user_id = self.get_user_by_ssrc(pckt.packet.ssrc);

                    let user_id_string = user_id
                        .clone()
                        .map(|user_id| user_id.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    trace!(?user_id_string, ?silent, len = pcm.len(), "got audio packet");
                }
            }
            songbird::EventContext::SpeakingUpdate(update) => {
                debug!(?update, "speaker updated")
            }
            songbird::EventContext::ClientDisconnect(event) => {
                self.remove_user_by_id(event.user_id.0);
                debug!(?event.user_id, "client disconnected");
            }
            _ => {
                debug!("unrecognised voice event");
            }
        };

        None
    }
}
