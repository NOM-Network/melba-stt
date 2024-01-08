use std::sync::Arc;

use serde::Serialize;
use serde_with::serde_as;
use serenity::futures::{SinkExt, StreamExt};
use tracing::{debug, error, trace};

use crate::config::Config;

pub struct Handler {
    config: Config,
    out_send: tokio::sync::mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
    speech_recv: tokio::sync::broadcast::Receiver<crate::stream::UserSegments>,
    voice_event_send: tokio::sync::broadcast::Sender<VoiceEvent>,
    voice_event_recv: tokio::sync::broadcast::Receiver<VoiceEvent>,
}

impl Handler {
    pub fn new(
        config: Config,
        out_send: tokio::sync::mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
        speech_recv: tokio::sync::broadcast::Receiver<crate::stream::UserSegments>,
        voice_event_send: tokio::sync::broadcast::Sender<VoiceEvent>,
        voice_event_recv: tokio::sync::broadcast::Receiver<VoiceEvent>,
    ) -> Self {
        Self {
            config,
            out_send,
            speech_recv,
            voice_event_send,
            voice_event_recv,
        }
    }

    pub fn connect(&self, cache: Arc<serenity::client::Cache>) -> Websocket {
        Websocket::new(
            self.config.clone(),
            self.out_send.clone(),
            self.speech_recv.resubscribe(),
            self.voice_event_recv.resubscribe(),
            cache,
        )
    }
}

#[serenity::async_trait]
impl serenity::client::EventHandler for Handler {
    async fn cache_ready(
        &self,
        ctx: serenity::prelude::Context,
        _: Vec<serenity::model::id::GuildId>,
    ) {
        let cache = ctx.cache.clone();
        let ws = self.connect(cache);

        tokio::task::spawn(async move {
            ws.run().await;
        });
    }

    async fn voice_channel_status_update(
        &self,
        _ctx: serenity::prelude::Context,
        old: Option<String>,
        status: Option<String>,
        channel_id: serenity::model::id::ChannelId,
        guild_id: serenity::model::id::GuildId,
    ) {
        self.voice_event_send
            .send(VoiceEvent::VoiceChannelStatusUpdate {
                old,
                status,
                channel_id,
                guild_id,
            })
            .unwrap();
    }

    async fn voice_state_update(
        &self,
        _ctx: serenity::prelude::Context,
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) {
        debug!(?old, ?new, "voice state update");
        self.voice_event_send
            .send(VoiceEvent::VoiceStateUpate { old, new })
            .unwrap();
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum VoiceEvent {
    #[serde(rename_all = "camelCase")]
    VoiceChannelStatusUpdate {
        old: Option<String>,
        status: Option<String>,
        channel_id: serenity::model::id::ChannelId,
        guild_id: serenity::model::id::GuildId,
    },
    #[serde(rename_all = "camelCase")]
    VoiceStateUpate {
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    },
}

pub struct Websocket {
    config: Config,
    out_send: tokio::sync::mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
    speech_recv: tokio::sync::broadcast::Receiver<crate::stream::UserSegments>,
    voice_event_recv: tokio::sync::broadcast::Receiver<VoiceEvent>,
    cache: Arc<serenity::cache::Cache>,
}

impl Websocket {
    pub fn new(
        config: Config,
        out_send: tokio::sync::mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
        speech_recv: tokio::sync::broadcast::Receiver<crate::stream::UserSegments>,
        voice_event_recv: tokio::sync::broadcast::Receiver<VoiceEvent>,
        cache: Arc<serenity::cache::Cache>,
    ) -> Self {
        Self {
            config,
            out_send,
            speech_recv,
            voice_event_recv,
            cache,
        }
    }

    pub async fn run(mut self) -> Self {
        let (mut websocket, response) =
            tokio_tungstenite::connect_async(self.config.ws.url.clone())
                .await
                .unwrap();

        debug!(?response, "connected to websocket");

        loop {
            let msg = tokio::select! {
                msg = websocket.next() => msg.map(|m| MessageSide::Websocket(m)),
                msg = self.speech_recv.recv() => msg.ok().map(|m| MessageSide::UnderstoodSpeech(m)),
                msg = self.voice_event_recv.recv() => msg.ok().map(|m| MessageSide::VoiceEvent(m)),
            };

            if let None = msg {
                break;
            }

            let msg = msg.unwrap();

            match msg {
                MessageSide::Websocket(msg) => {
                    if let Err(error) = msg {
                        error!(?error, "failed to read from websocket");
                        break;
                    }

                    let msg = msg.unwrap();

                    trace!(?msg, "got message from websocket");
                    self.out_send
                        .send(msg)
                        .await
                        .expect("failed to send msg out from websocket");
                }
                MessageSide::UnderstoodSpeech(speech) => {
                    trace!(?speech, "sending speech to websocket");

                    websocket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::to_string(
                                &TimestampedMessage::from_user_segments(
                                    speech,
                                    &self.config,
                                    self.cache.clone(),
                                )
                                .await,
                            )
                            .unwrap(),
                        ))
                        .await
                        .expect("failed to send message to websocket");
                }
                MessageSide::VoiceEvent(voice_event) => {
                    trace!(?voice_event, "sending voice event to websocket");

                    websocket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::to_string(&voice_event).unwrap(),
                        ))
                        .await
                        .expect("failed to send voice event to websocket");
                }
            }
        }

        self
    }
}

#[derive(Debug)]
enum MessageSide {
    Websocket(
        Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
    ),
    UnderstoodSpeech(crate::stream::UserSegments),
    VoiceEvent(VoiceEvent),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimestampedMessage {
    #[serde(with = "time::serde::iso8601")]
    timestamp: time::OffsetDateTime,
    #[serde(flatten)]
    inner: OutgoingMessage,
}

impl TimestampedMessage {
    async fn from_user_segments(
        user_segments: crate::stream::UserSegments,
        config: &Config,
        cache: Arc<serenity::cache::Cache>,
    ) -> Self {
        Self {
            timestamp: time::OffsetDateTime::now_utc(),
            inner: OutgoingMessage::Speech {
                user: UserInfo::from_id(user_segments.user_id, config, cache)
                    .await
                    .expect("unkown user"),
                speech: UnderstoodSpeech::from_parts(user_segments.segments, user_segments.timings),
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
#[serde(tag = "type")]
enum OutgoingMessage {
    Speech {
        user: UserInfo,
        #[serde(flatten)]
        speech: UnderstoodSpeech,
    },
}

#[derive(Debug, Serialize)]
#[serde_with::serde_as]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    #[serde_as(as = "serde_with::DisplayFromStr")]
    id: u64,
    username: String,
    global_name: Option<String>,
    nickname: Option<String>,
}

impl UserInfo {
    async fn from_id(
        id: serenity::model::id::UserId,
        config: &Config,
        cache: Arc<serenity::cache::Cache>,
    ) -> Option<Self> {
        let member = cache.member(config.stt.channel_to_join.guild_id, id)?;

        let info = Self {
            id: member.user.id.into(),
            username: member.user.name.clone(),
            global_name: member.user.global_name.clone(),
            nickname: member.nick.clone(),
        };

        Some(info)
    }
}

#[serde_as]
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct UnderstoodSpeech {
    #[serde(with = "time::serde::iso8601")]
    started_speaking_at: time::OffsetDateTime,
    #[serde_as(as = "serde_with::DurationSecondsWithFrac<f64>")]
    #[serde(rename = "speechSeconds")]
    speech_duration: time::Duration,
    #[serde_as(as = "serde_with::DurationSecondsWithFrac<f64>")]
    #[serde(rename = "processingSeconds")]
    processing_duration: time::Duration,
    speech_text: String,
    no_speech_prob: f32,
}

impl UnderstoodSpeech {
    fn from_parts(segments: Vec<nn::whisper::Segment>, timings: crate::stream::Timings) -> Self {
        let text = segments
            .iter()
            .map(|s| s.dr.text.to_owned())
            .intersperse(". ".to_string())
            .collect();

        let no_speech_prob = segments
            .iter()
            .map(|s| s.dr.no_speech_prob)
            .min_by(|l, r| l.partial_cmp(r).unwrap_or(std::cmp::Ordering::Less))
            .unwrap() as f32;

        Self {
            started_speaking_at: timings.started_listening_at,
            speech_duration: timings.done_listening_at - timings.started_listening_at,
            processing_duration: timings.done_prediction_at - timings.done_listening_at,
            speech_text: text,
            no_speech_prob,
        }
    }
}
