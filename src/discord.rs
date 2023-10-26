use std::sync::Arc;

use serenity::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, trace};

use crate::{
    audio::{ChannelSetup, StreamInfo},
    config::{self, SttConfig},
    nn::model::Segment,
};

pub async fn setup_discord_bot(
    token: String,
    config: Arc<SttConfig>,
) -> Result<(serenity::Client, Reciever, mpsc::Receiver<ChannelSetup>), Box<dyn std::error::Error>>
{
    use serenity::prelude::*;
    use songbird::SerenityInit;
    let (s, r) = mpsc::channel(8);
    let reciever = Reciever::new(s, config.clone());

    let songbird_config =
        songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode);

    let client = Client::builder(token, GatewayIntents::non_privileged())
        .event_handler(Handler {
            reciever: reciever.clone(),
            config: config.clone(),
            call: tokio::sync::Mutex::new(None),
        })
        .register_songbird_from_config(songbird_config)
        .await?;

    Ok((client, reciever, r))
}

struct Handler {
    reciever: Reciever,
    config: Arc<SttConfig>,
    call: tokio::sync::Mutex<Option<Arc<serenity::prelude::Mutex<songbird::Call>>>>,
}

#[async_trait]
impl serenity::client::EventHandler for Handler {
    async fn ready(&self, ctx: serenity::client::Context, ready: serenity::model::gateway::Ready) {
        info!(event=?ready, "ready");
        let manager = songbird::get(&ctx).await.expect("songbird failed").clone();
        let config::Channel {
            guild_id,
            channel_id,
        } = self.config.channel_to_join;
        let (handler_lock, connect_result) = manager.join(guild_id, channel_id).await;

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
        drop(handler);

        let mut inner_call = self.call.lock().await;
        *inner_call = Some(handler_lock);
    }
}

#[derive(Clone)]
pub struct Reciever {
    voice_ssrc_map: Arc<dashmap::DashMap<u32, u64>>,
    inverse_ssrc_map: Arc<dashmap::DashMap<u64, u32>>,
    ssrc_to_sender_map: Arc<dashmap::DashMap<u32, mpsc::Sender<Vec<i16>>>>,
    new_channel_send: Arc<mpsc::Sender<ChannelSetup>>,

    config: Arc<SttConfig>,
}

impl Reciever {
    fn new(setup_sender: mpsc::Sender<ChannelSetup>, config: Arc<SttConfig>) -> Self {
        Self {
            voice_ssrc_map: Arc::new(dashmap::DashMap::new()),
            inverse_ssrc_map: Arc::new(dashmap::DashMap::new()),
            ssrc_to_sender_map: Arc::new(dashmap::DashMap::new()),
            new_channel_send: Arc::new(setup_sender),
            config,
        }
    }

    async fn is_ssrc_ignored(&self, ssrc: u32) -> bool {
        let user_id = self.get_user_by_ssrc(ssrc).await;
        let user_id = match user_id {
            Some(user_id) => user_id,
            None => return false,
        };

        self.config.ignored_users.contains(&user_id)
    }

    async fn add_user_with_ssrc(&self, ssrc: u32, user_id: u64) {
        self.voice_ssrc_map.insert(ssrc, user_id);
        self.inverse_ssrc_map.insert(user_id, ssrc);

        let (sender, reciever) = mpsc::channel(128);
        self.new_channel_send
            .send(ChannelSetup {
                info: StreamInfo { user_id, ssrc },
                pcm_data_recv: reciever,
            })
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
                if self.is_ssrc_ignored(pckt.packet.ssrc).await {
                    return None;
                }
                if let Some(pcm) = pckt.audio {
                    if pcm.len() == 0 {
                        debug!(?pckt, "got zero length audio packet");
                    }

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
    pub message: Vec<Segment>,
    pub who: StreamInfo,
}
