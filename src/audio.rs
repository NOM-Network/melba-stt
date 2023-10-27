use futures::TryFutureExt;

use tokio::sync::{mpsc, oneshot, broadcast};
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    discord::CompletedMessage,
    nn::{model::Segment, ModelContainer},
};

pub const DISCORD_VOICE_SAMPLERATE_HZ: usize = 48_000;
pub const WHISPER_SAMPLERATE_HZ: usize = 16_000;

pub async fn handle_audio_streams(
    model: ModelContainer,
    mut channel_recv: mpsc::Receiver<ChannelSetup>,
    completed_message_send: broadcast::Sender<CompletedMessage>,
) {
    let mut tasks: Vec<RecvTask> = vec![];
    let dummy_task = tokio::task::spawn(async move {
        futures::future::pending::<()>().await;
        unreachable!("futures::future::pending should never return")
    });
    tasks.push(RecvTask {
        stream_info: StreamInfo {
            user_id: 0,
            ssrc: 0,
        },
        task: dummy_task,
    });
    info!("started handling streams");

    loop {
        // let channel_futures = channels.iter_mut().map(|c| c.recv()).collect::<Vec<_>>();
        let select_future = futures::future::select_all(&mut tasks);
        let result = tokio::select! {
            ch_recv = channel_recv.recv() => Ty::Init(ch_recv.expect("oopsie")),
            st_recv = select_future => Ty::Completed(st_recv.0),
        };
        let task_result = match result {
            Ty::Init(channel) => {
                // info!(channel=?channel.0, "recieved new channel");
                let stream_info = channel.info.clone();
                let completed_messages = completed_message_send.clone();
                let model = model.clone();
                let task = tokio::task::spawn(async move {
                    handle_stream(channel, completed_messages, model).await
                });
                tasks.push(RecvTask { stream_info, task });
                continue;
            }
            Ty::Completed(task) => task,
        };

        let info = match task_result {
            Ok(info) => info,
            Err(error) => {
                warn!(?error, "stream handle task exited with error");
                continue;
            }
        };

        tasks.retain(|recv| recv.stream_info != info);

        let StreamInfo { ssrc, user_id } = info;
        info!(?ssrc, ?user_id, "task ended");
        // let ((stream_info, packet), _index, _others) = select_future.await;
        // info!(?stream_info, len=packet.unwrap().len(), "got packet");
    }
}

#[instrument(skip_all)]
async fn handle_stream(
    setup: ChannelSetup,
    completed_message_send: broadcast::Sender<CompletedMessage>,
    model: ModelContainer,
) -> StreamInfo {
    let ChannelSetup {
        info: StreamInfo { user_id, ssrc },
        mut pcm_data_recv,
    } = setup;
    info!(ssrc, user_id, "starting recieve");
    let mut buffer = Vec::with_capacity((10000 / 20) * DISCORD_VOICE_SAMPLERATE_HZ); // pre-allocate space for roughly 10s of speech
    let mut started_filling_at = None;

    loop {
        let side = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => StreamSide::Silence,
            audio = pcm_data_recv.recv() => StreamSide::Audio(audio),
        };

        match side {
            StreamSide::Silence => {
                if buffer.is_empty() {
                    continue;
                }
                let out_buffer = Vec::from_iter(buffer.drain(..));
                info!(len = out_buffer.len(), "submitting audio");
                // todo!("submit audio to be processed");
                let model = model.clone();
                let completed_message_send = completed_message_send.clone();
                let info = setup.info.clone();
                tokio::task::spawn(async move {
                    let speech_duration = out_buffer.len() as f64 / DISCORD_VOICE_SAMPLERATE_HZ as f64;
                    let processed_text = process_buffer(out_buffer, model).await;
                    // info!(text=?processed_text, "processed result");
                    completed_message_send
                        .send(CompletedMessage {
                            started_at: started_filling_at.unwrap(),
                            speech_duration: std::time::Duration::from_secs_f64(speech_duration),
                            message: processed_text,
                            who: info,
                        })
                        .unwrap();
                });
                started_filling_at = None;
            }
            StreamSide::Audio(audio) => {
                if let None = started_filling_at {
                    started_filling_at = Some(time::OffsetDateTime::now_utc());
                }
                if let Some(audio) = audio {
                    buffer.extend(audio.into_iter().step_by(2));
                    trace!(buffer_len = buffer.len(), "appeneded")
                } else {
                    debug!(ssrc, user_id, "recieve stream closed");
                    break;
                }
            }
        }
    }

    StreamInfo { user_id, ssrc }
}

async fn process_buffer(buffer: Vec<i16>, model: ModelContainer) -> Vec<Segment> {
    let (from_thread_send, from_thread_recv) = oneshot::channel::<Vec<Segment>>();

    let _thread = std::thread::spawn(move || {
        sync_process_buffer(from_thread_send, buffer, model);
    });

    let speech = from_thread_recv.await.unwrap();

    speech
}

fn sync_process_buffer(
    responder: oneshot::Sender<Vec<Segment>>,
    buffer: Vec<i16>,
    model: ModelContainer,
) {
    let result = samplerate::convert(
        DISCORD_VOICE_SAMPLERATE_HZ as u32,
        WHISPER_SAMPLERATE_HZ as u32,
        1,
        samplerate::ConverterType::SincFastest,
        buffer
            .into_iter()
            .map(|v| v as f32 / i16::MAX as f32)
            .collect::<Vec<_>>()
            .as_slice(),
    );
    // let p_result = result.map(|result| result.len());
    // debug!(?p_result, "resampled auio");
    let prediction = model.predict(result.unwrap());
    responder.send(prediction).unwrap();
}

pub enum StreamSide {
    Silence,
    Audio(Option<Vec<i16>>),
}

pub struct RecvTask {
    stream_info: StreamInfo,
    task: tokio::task::JoinHandle<StreamInfo>,
}

impl futures::Future for RecvTask {
    type Output = Result<StreamInfo, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.task.try_poll_unpin(cx)
    }
}

pub enum Ty {
    Init(ChannelSetup),
    Completed(Result<StreamInfo, tokio::task::JoinError>),
}

pub struct ChannelSetup {
    pub info: StreamInfo,
    pub pcm_data_recv: mpsc::Receiver<Vec<i16>>,
}

impl futures::Future for ChannelSetup {
    type Output = (StreamInfo, Option<Vec<i16>>);

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.pcm_data_recv
            .poll_recv(cx)
            .map(|v| (self.info.clone(), v))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub user_id: u64,
    pub ssrc: u32,
}

#[derive(Debug, Clone)]
pub struct VoiceEvent {
    pub user_id: u64,
    pub timestamp: time::OffsetDateTime,
    pub event: Event,
}

#[derive(Debug, Clone)]
pub enum Event {
    Connect,
    SpeakForFirstTime,
    BeginSpeaking,
    EndSpeaking,
    Disconnect,
}