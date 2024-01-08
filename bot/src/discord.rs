use std::sync::Arc;

use dashmap::DashMap;
use serenity::{async_trait, client::Context, model::voice::VoiceState};
use tracing::{debug, instrument, trace, warn};

use crate::{config, stream::StreamProcessor};

pub async fn setup_discord_bot(
    token: &str,
    stream_processor: Arc<StreamProcessor>,
    recorder: recorder::Recorder,
    config: config::Config,
    completed_speech_recv: tokio::sync::broadcast::Receiver<crate::stream::UserSegments>,
) -> serenity::Client {
    use serenity::prelude::*;
    use songbird::SerenityInit;

    let songbird_config =
        songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Decode);

    let (ws_out_send, _ws_out_recv) = tokio::sync::mpsc::channel(16);
    let (voice_event_send, voice_event_recv) = tokio::sync::broadcast::channel(16);

    let ws_handler = crate::ws::Handler::new(
        config.clone(),
        ws_out_send,
        completed_speech_recv,
        voice_event_send,
        voice_event_recv,
    );

    Client::builder(
        token,
        GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS,
    )
    .event_handler(Handler {
        recorder,
        stream_processor,
        config: config.clone(),
    })
    .event_handler(ws_handler)
    .register_songbird_from_config(songbird_config)
    .await
    .expect("failed to create client")
}

struct Handler {
    stream_processor: Arc<StreamProcessor>,
    config: config::Config,
    recorder: recorder::Recorder,
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

        let recorder_session = self
            .recorder
            .session(
                format!(
                    "{}-{}",
                    self.config.stt.channel_to_join.channel_id,
                    time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
                )
                .into(),
            )
            .await;

        let manager = songbird::get(&ctx)
            .await
            .expect("failed to find songbird manager");

        let c = manager
            .join(
                serenity::model::id::GuildId::new(self.config.stt.channel_to_join.guild_id),
                serenity::model::id::ChannelId::new(self.config.stt.channel_to_join.channel_id),
            )
            .await
            .unwrap();
        let mut h = c.lock().await;
        let receiver = Receiver {
            send_map: Default::default(),
            ssrc_to_user_id: Default::default(),
            stream_proccessor: self.stream_processor.clone(),
            session: Arc::new(recorder_session),
        };
        h.add_global_event(
            songbird::CoreEvent::SpeakingStateUpdate.into(),
            receiver.clone(),
        );
        h.add_global_event(songbird::CoreEvent::VoiceTick.into(), receiver.clone());
        h.add_global_event(
            songbird::CoreEvent::ClientDisconnect.into(),
            receiver.clone(),
        );
    }
}

type Sample = i16;
type Ssrc = u32;
type UserId = u64;

#[derive(Clone)]
struct Receiver {
    send_map: Arc<DashMap<Ssrc, (tokio::sync::mpsc::Sender<Vec<i16>>, recorder::Sink)>>,
    ssrc_to_user_id: Arc<DashMap<Ssrc, UserId>>,
    stream_proccessor: Arc<StreamProcessor>,
    session: Arc<recorder::Session>,
}

#[async_trait]
impl songbird::EventHandler for Receiver {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match ctx {
            songbird::EventContext::SpeakingStateUpdate(speaking) => match speaking.user_id {
                Some(user_id) => {
                    trace!(?user_id, "spoke for first time");
                    self.ssrc_to_user_id.insert(speaking.ssrc, user_id.0);
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
                        let user_id = match self.ssrc_to_user_id.get(speaker_ssrc) {
                            Some(v) => v,
                            None => return None,
                        };
                        let user_id = serenity::model::id::UserId::new(*user_id.value());

                        debug!(ssrc = speaker_ssrc, ?user_id, "adding");
                        let (s, r) = tokio::sync::mpsc::channel(64);
                        self.send_map.insert(
                            *speaker_ssrc,
                            (
                                s,
                                self.session
                                    .new_speaker(
                                        format!(
                                            "{}-{}",
                                            user_id,
                                            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
                                        )
                                        .into(),
                                    )
                                    .listen()
                                    .await,
                            ),
                        );
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

                for kv in self
                    .send_map
                    .iter()
                    .filter(|kv| (!speakers.contains_key(kv.key())))
                {
                    let (_, sink) = kv.value();

                    sink.send_audio(recorder::Audio::Silence)
                        .await
                        .expect("failed to send silence");
                }
            }
            songbird::EventContext::ClientDisconnect(disconnect) => {
                let user_id = disconnect.user_id.0;
                let ssrc = self
                    .ssrc_to_user_id
                    .iter()
                    .filter_map(|r| (*r.value() == user_id).then(|| *r.key()))
                    .next();

                let ssrc = match ssrc {
                    Some(ssrc) => ssrc,
                    None => {
                        // if a user never spoke, don't try to save their speech
                        return None;
                    }
                };

                self.ssrc_to_user_id.remove(&ssrc);
                let (_, (_, sink)) = self.send_map.remove(&ssrc).unwrap();

                tokio::task::spawn(async move { sink.do_merge().await });
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
        let kv = self
            .send_map
            .get(&ssrc)
            .expect("somehow couldn't find sender");

        let (sender, sink) = kv.value();

        sink.send_audio(recorder::Audio::Packets(data.clone()))
            .await
            .expect("failed to record voice");

        sender
            .send_timeout(data, std::time::Duration::from_millis(10))
            .await
            .expect("failed to send voice data (did thread die?)");
    }
}
