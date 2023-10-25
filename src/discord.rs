use std::sync::Arc;

use serenity::async_trait;
use tracing::{debug, error, info, instrument, trace, warn};
use tokio::sync::mpsc;

use crate::audio::{StreamInfo, ChannelSetup};

pub async fn setup_discord_bot(
) -> Result<(serenity::Client, Reciever, mpsc::Receiver<ChannelSetup>), Box<dyn std::error::Error>>
{
    use serenity::prelude::*;
    use songbird::SerenityInit;
    let (s, r) = mpsc::channel(8);
    let reciever = Reciever::new(s);

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

    Ok((client, reciever, r))
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
        handler.add_global_event(
            songbird::CoreEvent::VoicePacket.into(),
            self.reciever.clone(),
        );
        handler.add_global_event(
            songbird::CoreEvent::SpeakingUpdate.into(),
            self.reciever.clone(),
        );
        handler.add_global_event(
            songbird::CoreEvent::ClientDisconnect.into(),
            self.reciever.clone(),
        );
        handler.add_global_event(
            songbird::CoreEvent::SpeakingStateUpdate.into(),
            self.reciever.clone(),
        );
    }
}

#[derive(Clone)]
pub struct Reciever {
    voice_ssrc_map: Arc<dashmap::DashMap<u32, u64>>,
    inverse_ssrc_map: Arc<dashmap::DashMap<u64, u32>>,
    ssrc_to_sender_map: Arc<dashmap::DashMap<u32, mpsc::Sender<Vec<i16>>>>,
    new_channel_send: Arc<mpsc::Sender<ChannelSetup>>,
}


impl Reciever {
    fn new(setup_sender: mpsc::Sender<ChannelSetup>) -> Self {
        Self {
            voice_ssrc_map: Arc::new(dashmap::DashMap::new()),
            inverse_ssrc_map: Arc::new(dashmap::DashMap::new()),
            ssrc_to_sender_map: Arc::new(dashmap::DashMap::new()),
            new_channel_send: Arc::new(setup_sender),
        }
    }

    async fn add_user_with_ssrc(&self, ssrc: u32, user_id: u64) {
        self.voice_ssrc_map.insert(ssrc, user_id);
        self.inverse_ssrc_map.insert(user_id, ssrc);

        let (sender, reciever) = mpsc::channel(128);
        self.new_channel_send
            .send(ChannelSetup { info: StreamInfo { user_id, ssrc }, pcm_data_recv: reciever })
            .await
            .unwrap();
        self.ssrc_to_sender_map.insert(ssrc, sender);
    }

    async fn get_user_by_ssrc(&self, ssrc: u32) -> Option<u64> {
        self.voice_ssrc_map.get(&ssrc).map(|kv| *kv.value())
    }

    async fn get_user_by_id(&self, user_id: u64) -> Option<u32> {
        self.inverse_ssrc_map.get(&user_id).map(|kv| *kv.value())
    }

    async fn remove_user_by_id(&self, user_id: u64) -> Option<u32> {
        if let Some(ssrc) = self.inverse_ssrc_map.get(&user_id) {
            let ssrc = ssrc.value();
            self.voice_ssrc_map.remove(ssrc);
            self.ssrc_to_sender_map.remove(ssrc);
            return Some(*ssrc);
        }
        None
    }

    async fn remove_user_by_ssrc(&self, ssrc: u32) -> Option<u64> {
        if let Some(user_id) = self.voice_ssrc_map.get(&ssrc) {
            let user_id = user_id.value();
            self.inverse_ssrc_map.remove(user_id);
            self.ssrc_to_sender_map.remove(&ssrc);
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
                self.add_user_with_ssrc(event.ssrc, event.user_id.unwrap().0)
                    .await;
            }
            songbird::EventContext::VoicePacket(pckt) => {
                if let Some(pcm) = pckt.audio {
                    // pckt.packet.ssrc
                    if pcm.len() == 0 {
                        debug!(?pckt, "got zero length audio packet");
                    }
                    let silent = pcm.iter().filter(|item| **item != 0i16).count() == 0;
                    let user_id = self.get_user_by_ssrc(pckt.packet.ssrc).await;

                    let user_id_string = user_id
                        .clone()
                        .map(|user_id| user_id.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let sender = self.ssrc_to_sender_map.get(&pckt.packet.ssrc);
                    if let Some(sender) = sender {
                        let sender = sender.value();
                        sender.send(pcm.to_owned()).await.expect("oopsie 2");
                    } else {
                        trace!(
                            srrc = pckt.packet.ssrc,
                            "dropped packet as no linked speaker"
                        )
                    }
                    // trace!(?user_id_string, ?silent, len = pcm.len(), "got audio packet");
                }
            }
            songbird::EventContext::SpeakingUpdate(update) => {
                debug!(?update, "speaker updated")
            }
            songbird::EventContext::ClientDisconnect(event) => {
                self.remove_user_by_id(event.user_id.0).await;
                debug!(?event.user_id, "client disconnected");
            }
            _ => {
                debug!("unrecognised voice event");
            }
        };

        None
    }
}

#[derive(Debug, Clone)]
pub struct CompletedMessage {
    pub message: String,
    pub who: StreamInfo,
}