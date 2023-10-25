use std::sync::Arc;

use actual_discord_stt::nn::NnPaths;

use futures::TryFutureExt;
use serenity::async_trait;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
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

    let (mut client, r, channel_init_recv) = setup_discord_bot().await?;

    let handle_streams_task =
        tokio::task::spawn(async move { handle_audio_streams(r, channel_init_recv).await });

    let timeout_fut = tokio::time::timeout(std::time::Duration::from_secs(30), client.start());
    info!("starting client");
    tokio::select! {
        _ = timeout_fut => (),
        _ = handle_streams_task => (),
    }
    info!("done");

    Ok(())
}

async fn handle_audio_streams(reciever: Reciever, mut channel_recv: mpsc::Receiver<ChannelSetup>) {
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
                info!(channel=?channel.0, "recieved new channel");
                let stream_info = channel.0.clone();
                let task = tokio::task::spawn(async move { handle_stream(channel).await });
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

    info!("done handling streams")
}

#[instrument(skip_all)]
async fn handle_stream(setup: ChannelSetup) -> StreamInfo {
    let ChannelSetup(StreamInfo { user_id, ssrc }, mut reciever) = setup;
    info!(ssrc, user_id, "starting recieve");
    let mut buffer = Vec::with_capacity((10000 / 20) * 48_000); // pre-allocate space for roughly 10s of speech

    loop {
        let side = tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => StreamSide::Silence,
            audio = reciever.recv() => StreamSide::Audio(audio),
        };

        match side {
            StreamSide::Silence => {
                if buffer.is_empty() {
                    continue;
                }
                let out_buffer = Vec::from_iter(buffer.drain(..));
                info!(len = out_buffer.len(), "submitting audio");
                // todo!("submit audio to be processed");
                let processed_text = process_buffer(out_buffer).await;
                info!(text=?processed_text, "processed result");
            }
            StreamSide::Audio(audio) => {
                if let Some(mut audio) = audio {
                    buffer.append(&mut audio);
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

async fn process_buffer(buffer: Vec<i16>) -> String {
    let (to_thread_send, mut to_thread_recv) =
        mpsc::channel::<(Vec<i16>, oneshot::Sender<String>)>(1);
    let (from_thread_send, from_thread_recv) = oneshot::channel::<String>();

    let _thread = std::thread::spawn(move || {
        while let Some((data, responder)) = to_thread_recv.blocking_recv() {
            let result = samplerate::convert(
                48_000,
                16_000,
                1,
                samplerate::ConverterType::SincFastest,
                data.into_iter()
                    .map(|v| v as f32 / i16::MAX as f32)
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            let p_result = result.map(|result| result.len());
            debug!(?p_result, "resampled auio");
            responder.send(":3".to_string()).unwrap();
        }
    });

    to_thread_send
        .send((buffer, from_thread_send))
        .await
        .unwrap();
    let speech = from_thread_recv.await.unwrap();

    speech
}

enum StreamSide {
    Silence,
    Audio(Option<Vec<i16>>),
}

struct RecvTask {
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

enum Ty {
    Init(ChannelSetup),
    Completed(Result<StreamInfo, tokio::task::JoinError>),
}
async fn setup_discord_bot(
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
struct Reciever {
    voice_ssrc_map: Arc<dashmap::DashMap<u32, u64>>,
    inverse_ssrc_map: Arc<dashmap::DashMap<u64, u32>>,
    new_channel_send: Arc<mpsc::Sender<ChannelSetup>>,
    ssrc_to_sender_map: Arc<dashmap::DashMap<u32, mpsc::Sender<Vec<i16>>>>,
}

struct ChannelSetup(StreamInfo, mpsc::Receiver<Vec<i16>>);

// impl ChannelSetup {
//     async fn recv(&mut self) -> Option<Vec<i16>> {
//         self.1.recv().await
//     }
// }

impl futures::Future for ChannelSetup {
    type Output = (StreamInfo, Option<Vec<i16>>);

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.1.poll_recv(cx).map(|v| (self.0.clone(), v))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamInfo {
    user_id: u64,
    ssrc: u32,
}

impl Reciever {
    fn new(sender: mpsc::Sender<ChannelSetup>) -> Self {
        Self {
            voice_ssrc_map: Arc::new(dashmap::DashMap::new()),
            inverse_ssrc_map: Arc::new(dashmap::DashMap::new()),
            ssrc_to_sender_map: Arc::new(dashmap::DashMap::new()),
            new_channel_send: Arc::new(sender),
        }
    }

    async fn add_user_with_ssrc(&self, ssrc: u32, user_id: u64) {
        self.voice_ssrc_map.insert(ssrc, user_id);
        self.inverse_ssrc_map.insert(user_id, ssrc);

        let (sender, reciever) = mpsc::channel(128);
        self.new_channel_send
            .send(ChannelSetup(StreamInfo { user_id, ssrc }, reciever))
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
