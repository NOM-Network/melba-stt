use std::sync::Arc;

use itertools::Itertools;
use nn::{
    iterator::SpectrogramIteratorExt,
    model::ModelContainer,
};
use tracing::{debug, info};

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
    let resample_chunk_size = 20_000;

    let resampler = rubato::FastFixedIn::new(
        WHISPER_SAMPLERATE_HZ as f64 / DISCORD_SAMPLERATE_HZ as f64,
        1.0,
        rubato::PolynomialDegree::Cubic,
        resample_chunk_size,
        1,
    )
    .unwrap();

    let planner = realfft::RealFftPlanner::new();

    let filters = &processor.filters;

    let mut spectrogram = recv
        .blocking_iter()
        .flatten()
        .map(|sample| sample as f32 / i16::MAX as f32)
        .pad_extend(WHISPER_SAMPLERATE_HZ * 30 * 3, 0.0)
        .chunks(resample_chunk_size)
        .into_iter()
        .map(|chunk| chunk.into_iter().collect::<Vec<_>>())
        .map(|mut chunk| {
            if chunk.len() < resample_chunk_size {
                chunk.resize(resample_chunk_size, 0.0);
            }
            chunk
        })
        .resample(resampler)
        .flatten()
        .spectrogram(planner)
        .map(|v| {
            (0..80)
                .map(|i| {
                    filters[i * 201..(i + 1) * 201]
                        .iter()
                        .zip(v.iter())
                        .map(|(v, f)| *v * *f)
                        .sum::<f32>()
                        .max(1e-10)
                        .log10()
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mel_max = spectrogram
        .iter()
        .flatten()
        .max_by(|&l, &r| l.partial_cmp(r).unwrap_or(std::cmp::Ordering::Greater))
        .unwrap_or(&0.0)
        - 8.0;

    spectrogram
        .iter_mut()
        .for_each(|m| m.iter_mut().for_each(|m| *m = mel_max.max(*m) / 4.0 + 1.0));

    let prediction = processor.predict_with_mel(spectrogram);
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

struct ResamplingIterator<I, R>
where
    I: Iterator<Item = Vec<f32>>,
    R: rubato::Resampler<f32>,
{
    inner: I,
    resampler: R,
    out_buffer: Vec<Vec<f32>>,
}

impl<I, R> ResamplingIterator<I, R>
where
    I: Iterator<Item = Vec<f32>>,
    R: rubato::Resampler<f32>,
{
    fn new(inner: I, resampler: R) -> Self {
        let out_buffer = resampler.output_buffer_allocate(true);
        Self {
            inner,
            resampler,
            out_buffer,
        }
    }
}

impl<I, R> Iterator for ResamplingIterator<I, R>
where
    I: Iterator<Item = Vec<f32>>,
    R: rubato::Resampler<f32>,
{
    type Item = Vec<f32>;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.inner.next()?;
        self.resampler
            .process_into_buffer(&[&chunk], &mut self.out_buffer, None)
            .expect("failed to resample");
        Some(self.out_buffer[0].clone())
    }
}

trait ResamplingIteratorExt<I, R>
where
    I: Iterator<Item = Vec<f32>>,
    R: rubato::Resampler<f32>,
{
    fn resample(self, resampler: R) -> ResamplingIterator<I, R>;
}

impl<I: Iterator<Item = Vec<f32>>, R> ResamplingIteratorExt<I, R> for I
where
    R: rubato::Resampler<f32>,
{
    fn resample(self, resampler: R) -> ResamplingIterator<I, R> {
        ResamplingIterator::new(self, resampler)
    }
}

struct PaddingIterator<I>
where
    I: Iterator<Item = f32>,
{
    inner: I,
    remaining: usize,
    pad_size: usize,
    pad_value: f32,
}

impl<I> PaddingIterator<I>
where
    I: Iterator<Item = f32>,
{
    fn new(inner: I, pad_size: usize, pad_value: f32) -> Self {
        Self {
            inner,
            pad_value,
            remaining: pad_size,
            pad_size: pad_size,
        }
    }
}

impl<I> Iterator for PaddingIterator<I>
where
    I: Iterator<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let next_value = self.inner.next();

        self.remaining -= 1;
        
        match next_value {
            Some(value) => {
                if self.remaining == 0 {
                    self.remaining = self.pad_size;
                }

                Some(value)
            },
            None => if self.remaining > 0 {
                Some(self.pad_value)
            } else {
                None
            },
        }
    }
}

trait PaddingIteratorExt<I>
where
    I: Iterator<Item = f32>,
{
    fn pad_extend(self, pad_size: usize, pad_value: f32) -> PaddingIterator<I>;
}

impl<I> PaddingIteratorExt<I> for I
where
    I: Iterator<Item = f32>,
{
    fn pad_extend(self, pad_size: usize, pad_value: f32) -> PaddingIterator<I> {
        PaddingIterator::new(self, pad_size, pad_value)
    }
}
