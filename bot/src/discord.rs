use std::sync::Arc;

use dashmap::DashMap;
use serenity::{
    async_trait,
    client::Context,
    model::voice::VoiceState,
};
use tracing::{debug, trace, warn, instrument};

use crate::{config, stream::StreamProcessor};

pub async fn setup_discord_bot(
    token: &str,
    stream_processor: Arc<StreamProcessor>,
    config: config::SttConfig,
) -> serenity::Client {
    use serenity::prelude::*;
    use songbird::SerenityInit;

    let songbird_config =
        songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode);

    Client::builder(
        token,
        GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS,
    )
    .event_handler(Handler {
        stream_processor,
        config,
    })
    .register_songbird_from_config(songbird_config)
    .await
    .expect("failed to create client")
}

struct Handler {
    stream_processor: Arc<StreamProcessor>,
    config: config::SttConfig,
}

#[async_trait]
impl serenity::client::EventHandler for Handler {
    async fn ready(&self, _: Context, ready: serenity::model::gateway::Ready) {
        debug!(user=?ready.user, "got ready message");
    }

    async fn cache_ready(
        &self,
        ctx: serenity::client::Context,
        _: Vec<serenity::model::id::GuildId>,
    ) {
        debug!("cache ready");

        let manager = songbird::get(&ctx)
            .await
            .expect("failed to find songbird manager");

        let c = manager
            .join(
                serenity::model::id::GuildId::new(self.config.channel_to_join.guild_id),
                serenity::model::id::ChannelId::new(self.config.channel_to_join.channel_id),
            )
            .await
            .unwrap();
        let mut h = c.lock().await;
        let receiver = Receiver {
            send_map: Default::default(),
            unready_map: Default::default(),
            stream_proccessor: self.stream_processor.clone(),
        };
        h.add_global_event(
            songbird::CoreEvent::SpeakingStateUpdate.into(),
            receiver.clone(),
        );
        h.add_global_event(songbird::CoreEvent::VoiceTick.into(), receiver.clone());
    }

    async fn voice_state_update(&self, _: Context, old: Option<VoiceState>, new: VoiceState) {
        trace!(?old, ?new, "update");
        // todo: log to websocket, though a seperate event handler for that would be nice to reduce clutter
    }
}

type Sample = i16;
type Ssrc = u32;
type UserId = u64;

#[derive(Clone)]
struct Receiver {
    send_map: Arc<DashMap<Ssrc, tokio::sync::mpsc::Sender<Vec<i16>>>>,
    unready_map: Arc<DashMap<Ssrc, UserId>>,
    stream_proccessor: Arc<StreamProcessor>,
}

#[async_trait]
impl songbird::EventHandler for Receiver {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match ctx {
            songbird::EventContext::SpeakingStateUpdate(speaking) => match speaking.user_id {
                Some(user_id) => {
                    trace!(?user_id, "spoke for first time");
                    self.unready_map.insert(speaking.ssrc, user_id.0);
                }
                None => {
                    warn!(
                        ssrc = speaking.ssrc,
                        "got speaking state update without user id"
                    )
                }
            },
            songbird::EventContext::VoiceTick(tick) => {
                let speakers = &tick.speaking;

                for (speaker_ssrc, data) in speakers {
                    if !self.send_map.contains_key(speaker_ssrc) {
                        let user_id = match self.unready_map.get(speaker_ssrc) {
                            Some(v) => v,
                            None => return None,
                        };
                        let user_id = serenity::model::id::UserId::new(*user_id.value());

                        debug!(ssrc = speaker_ssrc, ?user_id, "adding");
                        let (s, r) = tokio::sync::mpsc::channel(64);
                        self.send_map.insert(*speaker_ssrc, s);
                        let stream_processor = self.stream_proccessor.clone();
                        tokio::task::spawn(async move {
                            stream_processor.listen_to_user(user_id, r).await;
                        });
                    }

                    let pcm = data
                        .decoded_voice
                        .clone()
                        .unwrap()
                        .into_iter()
                        .step_by(2) // audio is interleaved LRLRLR, only take the left channel
                        .collect::<Vec<_>>();

                    self.ssrc_spoke(*speaker_ssrc, pcm).await;
                }
            }
            other => {
                trace!(?other, "unknown event")
            }
        };

        None
    }
}

impl Receiver {
    #[instrument(skip(self, data), level = "trace")]
    async fn ssrc_spoke(&self, ssrc: u32, data: Vec<Sample>) {
        let sender = self.send_map.get(&ssrc).expect("somehow couldn't find sender");
        sender
            .send_timeout(data, std::time::Duration::from_millis(10))
            .await
            .expect("failed to send voice data (did thread die?)");
    }
}
