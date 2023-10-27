use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite;
use tracing::{debug, info};

use crate::{
    audio::{self, VoiceEvent},
    config,
    discord::CompletedMessage,
};

pub async fn websocket_task(
    mut completed_messages: broadcast::Receiver<CompletedMessage>,
    mut speaking_events: broadcast::Receiver<VoiceEvent>,
    cache: Arc<serenity::cache::Cache>,
    config: Arc<config::SttConfig>,
) {
    let (mut websocket, response) = tokio_tungstenite::connect_async(config.ws_url.clone())
        .await
        .expect("websocket failed to connect");

    debug!(?response, "connected to websocket");

    loop {
        let msg = tokio::select! {
            msg = websocket.next() => MessageSide::Websocket(msg),
            msg = completed_messages.recv() => MessageSide::UnderstoodSpeech(msg),
            msg = speaking_events.recv() => MessageSide::VoiceEvent(msg),
        };

        match msg {
            MessageSide::Websocket(msg) => {
                let message = msg.unwrap().unwrap();
                info!(?message, "got message on websocket");
            }
            MessageSide::UnderstoodSpeech(msg) => {
                let message = TimestampedUserMessage::from_completed_message(
                    msg.unwrap(),
                    cache.clone(),
                    config.clone(),
                );
                websocket
                    .send(tungstenite::Message::Text(
                        serde_json::to_string(&message).unwrap(),
                    ))
                    .await
                    .expect("failed to send over websocket");
            }
            MessageSide::VoiceEvent(msg) => {
                let message = msg.unwrap();
                let message = TimestampedUserMessage::from_voice_event(
                    message,
                    cache.clone(),
                    config.clone(),
                );
                websocket
                    .send(tungstenite::Message::Text(
                        serde_json::to_string(&message).expect("failed to serialize json"),
                    ))
                    .await
                    .expect("failed to send voice event over websocket");
            }
        }
    }
}

enum MessageSide {
    Websocket(Option<Result<tungstenite::Message, tungstenite::Error>>),
    UnderstoodSpeech(Result<CompletedMessage, broadcast::error::RecvError>),
    VoiceEvent(Result<VoiceEvent, broadcast::error::RecvError>),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimestampedUserMessage {
    user: UserInfo,
    #[serde(with = "time::serde::iso8601")]
    timestamp: time::OffsetDateTime,
    #[serde(flatten)]
    inner: OutgoingWebsocketMessage,
}

impl TimestampedUserMessage {
    fn from_completed_message(
        message: CompletedMessage,
        cache: Arc<serenity::cache::Cache>,
        config: Arc<config::SttConfig>,
    ) -> Self {
        let username = cache
            .member(config.channel_to_join.guild_id, message.who.user_id)
            .expect("unkown user spoke somehow")
            .user
            .name;

        let message_text = message
            .message
            .iter()
            .map(|segment| segment.dr.text.to_owned())
            .collect::<Vec<String>>()
            .join(" ");

        let min_no_speech_prob = message
            .message
            .iter()
            .map(|segement| (segement.dr.no_speech_prob * 1_000_000.0) as u32)
            .min()
            .map(|value| (value as f64) / 1_000_000.0)
            .expect("somehow had no segments in speech");

        Self {
            user: UserInfo {
                id: message.who.user_id,
                username,
            },
            timestamp: time::OffsetDateTime::now_utc(),
            inner: OutgoingWebsocketMessage::UnderstoodSpeech {
                started_speaking_at: message.started_at,
                speech_duration: message.speech_duration.as_secs_f64(),
                speech_text: message_text.trim().to_string(),
                no_speech_probability: min_no_speech_prob,
            },
        }
    }

    fn from_voice_event(
        event: VoiceEvent,
        cache: Arc<serenity::cache::Cache>,
        config: Arc<config::SttConfig>,
    ) -> Self {
        let username = cache
            .member(config.channel_to_join.guild_id, event.who.user_id)
            .expect("user appeared in voice event with unkown user id")
            .user
            .name;

        Self {
            user: UserInfo {
                id: event.who.user_id,
                username,
            },
            timestamp: event.timestamp,
            inner: event.event.into(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OutgoingWebsocketMessage {
    UserSpokeForFirstTime,
    SpeakingBegin,
    SpeakingEnd,
    UserDisconnected,
    #[serde(rename_all = "camelCase")]
    UnderstoodSpeech {
        #[serde(with = "time::serde::iso8601")]
        started_speaking_at: time::OffsetDateTime,
        speech_duration: f64,
        speech_text: String,
        no_speech_probability: f64,
    },
}

impl From<audio::Event> for OutgoingWebsocketMessage {
    fn from(value: audio::Event) -> Self {
        match value {
            audio::Event::SpeakForFirstTime => Self::UserSpokeForFirstTime,
            audio::Event::BeginSpeaking => Self::SpeakingBegin,
            audio::Event::EndSpeaking => Self::SpeakingEnd,
            audio::Event::Disconnect => Self::UserDisconnected,
        }
    }
}

#[serde_with::serde_as]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    #[serde_as(as = "serde_with::DisplayFromStr")]
    id: u64,
    username: String,
}

#[derive(Debug, Deserialize)]
enum IncomingWebsocketMessage {
    // not currently expecting any messages to be sent from the websocket
}
