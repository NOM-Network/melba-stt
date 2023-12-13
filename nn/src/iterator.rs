use std::collections::VecDeque;

use candle_transformers::models::{whisper as whisper_constants, whisper::audio as whisper_audio};
use num_complex::ComplexFloat;
use realfft::RealFftPlanner;

pub const FFT_CHUNK_SIZE: usize = whisper_constants::N_FFT;
pub const FFT_BINS: usize = 80;

pub trait Float: realfft::FftNum + realfft::num_traits::Float + whisper_audio::Float {}
impl Float for f64 {}
impl Float for f32 {}

pub struct SpectrogramIterator<I, T>
where
    I: Iterator<Item = Vec<T>>,
    T: Float,
{
    inner: I,
    planner: realfft::RealFftPlanner<T>,
    hann_window: Vec<T>,
}

impl<I, T> SpectrogramIterator<I, T>
where
    I: Iterator<Item = Vec<T>>,
    T: Float,
{
    fn new(inner: I, planner: RealFftPlanner<T>) -> Self {
        Self {
            inner,
            planner,
            hann_window: hann_window(whisper_constants::N_FFT), // todo: make this modular
        }
    }
}

impl<I, T> Iterator for SpectrogramIterator<I, T>
where
    I: Iterator<Item = Vec<T>>,
    T: Float,
{
    type Item = Vec<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut samples = self.inner.next()?;
        samples
            .iter_mut()
            .zip(self.hann_window.iter())
            .for_each(|(sample, window)| *sample *= *window);

        let executor = self.planner.plan_fft_forward(whisper_constants::N_FFT);

        let zero = T::from(0.0).unwrap();
        let mut output =
            vec![realfft::num_complex::Complex::new(zero, zero); whisper_constants::N_FFT / 2 + 1];
        executor
            .process(&mut samples, &mut output)
            .expect("failed to process fft");

        let output = output
            .into_iter()
            .map(|c| c.abs().powi(2))
            .collect();
        Some(output)
    }
}

pub trait SpectrogramIteratorExt<I, T>
where
    I: Iterator<Item = T>,
    T: Float,
{
    fn spectrogram(
        self,
        planner: realfft::RealFftPlanner<T>,
    ) -> SpectrogramIterator<impl Iterator<Item = Vec<T>>, T>;
}

impl<I, T> SpectrogramIteratorExt<I, T> for I
where
    T: Float,
    I: Iterator<Item = T>,
    // R: Iterator<Item = Vec<T>>,
{
    fn spectrogram(
        self,
        planner: realfft::RealFftPlanner<T>,
    ) -> SpectrogramIterator<impl Iterator<Item = Vec<T>>, T> {
        let inner = self
            .into_iter()
            .ovelapping_windows(whisper_constants::HOP_LENGTH, whisper_constants::N_FFT);
        SpectrogramIterator::new(inner, planner)
    }
}

fn hann_window<T>(size: usize) -> Vec<T>
where
    T: Float,
{
    let two_pi = T::PI() + T::PI();
    let half = T::from(0.5).unwrap();
    let one = T::from(1.0).unwrap();
    let fft_size_t = T::from(size).unwrap();

    (0..size)
        .map(|i| half * (one - ((two_pi * T::from(i).unwrap()) / fft_size_t).cos()))
        .collect()
}

pub struct OverlappingWindows<I, T>
where
    I: Iterator<Item = T>,
    T: Float,
{
    inner: I,
    buffer: VecDeque<T>,
    step_size: usize,
    window_size: usize,
}

impl<I, T> OverlappingWindows<I, T>
where
    I: Iterator<Item = T>,
    T: Float,
{
    fn new(inner: I, step_by: usize, window_size: usize) -> Self {
        Self {
            inner,
            buffer: VecDeque::with_capacity(window_size),
            step_size: step_by,
            window_size,
        }
    }
}

impl<I, T> Iterator for OverlappingWindows<I, T>
where
    I: Iterator<Item = T>,
    T: Float,
{
    type Item = Vec<T>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(item) = self.inner.next() {
            self.buffer.push_back(item);

            if self.buffer.len() >= self.window_size {
                let value = self
                    .buffer
                    .iter()
                    .take(self.window_size)
                    .cloned()
                    .collect::<Vec<_>>();
                let _ = self.buffer.drain(..self.step_size);
                return Some(value);
            }
        }
        None
    }
}

pub trait OverlappingWindowsExt<I, T>
where
    I: Iterator<Item = T>,
    T: Float,
{
    fn ovelapping_windows(self, step_by: usize, window_size: usize) -> OverlappingWindows<I, T>;
}

impl<I: Iterator<Item = T>, T> OverlappingWindowsExt<I, T> for I
where
    T: Float,
{
    fn ovelapping_windows(self, step_by: usize, window_size: usize) -> OverlappingWindows<I, T> {
        OverlappingWindows::new(self, step_by, window_size)
    }
}
