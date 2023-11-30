use std::sync::Arc;

use nn::model::ModelContainer;
use tracing::{debug, info, warn};

const DISCORD_SAMPLERATE_HZ: usize = 48_000;
const WHISPER_SAMPLERATE_HZ: usize = 16_000;

pub struct StreamProcessor {
    model: Arc<ModelContainer>,
}

impl StreamProcessor {
    pub fn new(model: ModelContainer) -> Self {
        Self {
            model: Arc::new(model),
        }
    }

    pub async fn listen_to_user(
        &self,
        user_id: serenity::model::id::UserId,
        mut receiver: tokio::sync::mpsc::Receiver<Vec<i16>>,
    ) {
        let mut current_sender = None;

        info!(?user_id, "starting listen");

        while let Some(side) = tokio::select! {
            r = receiver.recv() => r.map(|v| StreamSide::Speech(v)),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => Some(StreamSide::Silence),
        } {
            match side {
                StreamSide::Silence => {
                    if let Some(_) = current_sender {
                        current_sender = None;
                        debug!(?user_id, "silence")
                    }
                }
                StreamSide::Speech(pcm) => {
                    let sender = match current_sender.clone() {
                        Some(sender) => sender,
                        None => {
                            info!(?user_id, "started processing");
                            let (speech_s, speech_r) = tokio::sync::mpsc::channel(1024);
                            current_sender = Some(speech_s.clone());
                            let processor = self.model.get_new_speaker();
                            std::thread::spawn(|| handle_speech(speech_r, processor));

                            speech_s
                        }
                    };

                    sender
                        .send(pcm)
                        .await
                        .expect("fixme: failed to send to channel that should still be open");
                }
            }
        }
    }
}

enum StreamSide {
    Silence,
    Speech(Vec<i16>),
}

fn handle_speech(
    recv: tokio::sync::mpsc::Receiver<Vec<i16>>,
    mut processor: nn::model::SpeakerProcessor,
) {
    let samples = recv.blocking_iter()
        .flatten()
        .map(|sample| sample as f32 / i16::MAX as f32)
        .collect::<Vec<_>>();
    let samples = samplerate::convert(
        DISCORD_SAMPLERATE_HZ as u32,
        WHISPER_SAMPLERATE_HZ as u32,
        1,
        samplerate::ConverterType::SincFastest,
        &samples,
    ).unwrap();

    let prediction = processor.predict(samples);
    info!(?prediction, "completed speech");

    debug!("done listening");
}

struct BlockingMpscIterator<T> {
    inner: tokio::sync::mpsc::Receiver<T>,
}

impl<I> Iterator for BlockingMpscIterator<I> {
    type Item = I;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.blocking_recv()
    }
}

trait BlockingMpscIteratorExt<I> {
    fn blocking_iter(self) -> BlockingMpscIterator<I>;
}

impl<I> BlockingMpscIteratorExt<I> for tokio::sync::mpsc::Receiver<I> {
    fn blocking_iter(self) -> BlockingMpscIterator<I> {
        BlockingMpscIterator { inner: self }
    }
}
