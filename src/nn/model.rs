use std::sync::Mutex;

use candle_core::IndexOp;
use candle_nn::ops::softmax;
use candle_transformers::models::whisper;
use rand::{SeedableRng, prelude::Distribution};

use tracing::{debug, error, info, instrument, trace, warn, trace_span};

use super::w_reimpl;

#[derive(Debug, Clone)]
pub struct DecodingResult {
    pub tokens: Vec<u32>,
    pub text: String,
    pub avg_logprob: f64,
    pub no_speech_prob: f64,
    pub temperature: f64,
    pub compression_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub duration: f64,
    pub dr: DecodingResult,
}

pub struct Decoder {
    model: w_reimpl::Whisper,
    rng: Mutex<rand::rngs::StdRng>,
    tokenizer: tokenizers::Tokenizer,
    suppress_tokens: candle_core::Tensor,
    sot_token: u32,
    transcribe_token: u32,
    translate_token: u32,
    eot_token: u32,
    no_speech_token: u32,
    no_timestamps_token: u32,
    language_token: Option<u32>,
}

impl Decoder {
    pub fn new(
        model: w_reimpl::Whisper,
        device: candle_core::Device,
        tokenizer: tokenizers::Tokenizer,
        seed: u64,
        language_token: Option<u32>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let no_timestamps_token = tokenizer.token_to_id(whisper::NO_TIMESTAMPS_TOKEN).unwrap();
        let suppress_tokens = (0..model.config.vocab_size as u32)
            .map(|i| {
                if model.config.suppress_tokens.contains(&i) || i == no_timestamps_token {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect::<Vec<f32>>();
        let suppress_tokens = candle_core::Tensor::new(suppress_tokens.as_slice(), &device)?;
        let sot_token = tokenizer.token_to_id(whisper::SOT_TOKEN).unwrap();
        let transcribe_token = tokenizer.token_to_id(whisper::TRANSCRIBE_TOKEN).unwrap();
        let translate_token = tokenizer.token_to_id(whisper::TRANSLATE_TOKEN).unwrap();
        let eot_token = tokenizer.token_to_id(whisper::EOT_TOKEN).unwrap();
        let no_speech_token = tokenizer.token_to_id(whisper::NO_SPEECH_TOKEN).unwrap();

        Ok(Self {
            model,
            rng: Mutex::new(rand::rngs::StdRng::seed_from_u64(seed)),
            tokenizer,
            suppress_tokens,
            sot_token,
            transcribe_token,
            translate_token,
            eot_token,
            no_speech_token,
            language_token,
            no_timestamps_token,
        })
    }

    #[instrument(skip_all)]
    fn decode(
        &self,
        mel: &candle_core::Tensor,
        t: f64,
    ) -> Result<DecodingResult, Box<dyn std::error::Error>> {
        let st = std::time::Instant::now();
        let audio_features = self.model.encoder.forward(mel, true)?;
        trace!(elapsed=?st.elapsed(), "audio features: {:?}", audio_features.dims());
        let sample_len = self.model.config.max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![self.sot_token];
        if let Some(language_token) = self.language_token {
            tokens.push(language_token);
        }
        tokens.push(self.transcribe_token);
        tokens.push(self.no_timestamps_token);

        for i in 0..sample_len {
            let tkns_st = std::time::Instant::now();
            let tokens_t = candle_core::Tensor::new(tokens.as_slice(), mel.device())?;
            let tkns_dur = tkns_st.elapsed();

            // The model expects a batch dim but this inference loop does not handle
            // it so we add it at this point.
            let fw_st = std::time::Instant::now();
            let tokens_t = tokens_t.unsqueeze(0)?;
            let ys = self.model.decoder.forward(&tokens_t, &audio_features, i == 0)?;
            let fw_dur = fw_st.elapsed();

            let nsp_start = std::time::Instant::now();
            // Extract the no speech probability on the first iteration by looking at the first
            // token logits and the probability for the according token.
            if i == 0 {
                let logits = self.model.decoder.final_linear(&ys.i(..1)?)?.i(0)?.i(0)?;
                no_speech_prob = candle_nn::ops::softmax(&logits, 0)?
                    .i(self.no_speech_token as usize)?
                    .to_scalar::<f32>()? as f64;
            }
            let nsp_dur = nsp_start.elapsed();

            let idk_st = std::time::Instant::now();
            let (_, seq_len, _) = ys.dims3()?;
            let logits = self.model
                .decoder
                .final_linear(&ys.i((..1, seq_len - 1..))?)?
                .i(0)?
                .i(0)?;
            // TODO: Besides suppress tokens, we should apply the heuristics from
            // ApplyTimestampRules, i.e.:
            // - Timestamps come in pairs, except before EOT.
            // - Timestamps should be non-decreasing.
            // - If the sum of the probabilities of timestamps is higher than any other tokens,
            //   only consider timestamps when sampling.
            // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L439
            let logits = logits.broadcast_add(&self.suppress_tokens)?;
            let next_token = if t > 0f64 {
                let prs = softmax(&(&logits / t)?, 0)?;
                let logits_v: Vec<f32> = prs.to_vec1()?;
                let distr = rand::distributions::WeightedIndex::new(&logits_v)?;
                let mut rng = self.rng.lock().unwrap();
                distr.sample(&mut *rng) as u32
            } else {
                let logits_v: Vec<f32> = logits.to_vec1()?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            let idk_dur = idk_st.elapsed();
            trace!(
                ?tkns_dur,
                ?fw_dur,
                ?nsp_dur,
                ?idk_dur,
                ?next_token,
                "found next token"
            );
            tokens.push(next_token);
            let prob = softmax(&logits, candle_core::D::Minus1)?
                .i(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            if next_token == self.eot_token || tokens.len() > self.model.config.max_target_positions {
                break;
            }
            sum_logprob += prob.ln();
        }
        let text = self.tokenizer.decode(&tokens, true).unwrap();
        let avg_logprob = sum_logprob / tokens.len() as f64;

        Ok(DecodingResult {
            tokens,
            text,
            avg_logprob,
            no_speech_prob,
            temperature: t,
            compression_ratio: f64::NAN,
        })
    }

    #[instrument(skip_all)]
    fn decode_with_fallback(
        &self,
        segment: &candle_core::Tensor,
    ) -> Result<DecodingResult, Box<dyn std::error::Error>> {
        for (i, &t) in whisper::TEMPERATURES.iter().enumerate() {
            let dr: Result<DecodingResult, _> = self.decode(segment, t);
            if i == whisper::TEMPERATURES.len() - 1 {
                return dr;
            }

            // On errors, we try again with a different temperature.
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio
                        > whisper::COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < whisper::LOGPROB_THRESHOLD;
                    if !needs_fallback || dr.no_speech_prob > whisper::NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    println!("Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    pub fn run(
        &self,
        mel: &candle_core::Tensor,
    ) -> Result<Vec<Segment>, Box<dyn std::error::Error>> {
        let (_, _, content_frames) = mel.dims3()?;
        let mut seek = 0;
        let mut segments = vec![];
        while seek < content_frames {
            let span = trace_span!("read_section");
            let _guard = span.enter();
            let start = std::time::Instant::now();
            let time_offset = (seek * whisper::HOP_LENGTH) as f64 / whisper::SAMPLE_RATE as f64;
            let segment_size = usize::min(content_frames - seek, whisper::N_FRAMES);
            let mel_segment = mel.narrow(2, seek, segment_size)?;
            let segment_duration =
                (segment_size * whisper::HOP_LENGTH) as f64 / whisper::SAMPLE_RATE as f64;
            let dr = self.decode_with_fallback(&mel_segment)?;
            seek += segment_size;
            if dr.no_speech_prob > whisper::NO_SPEECH_THRESHOLD
                && dr.avg_logprob < whisper::LOGPROB_THRESHOLD
            {
                info!("no speech detected, skipping {seek} {dr:?}");
                continue;
            }
            let segment = Segment {
                start: time_offset,
                duration: segment_duration,
                dr,
            };
            debug!(
                "{:.1}s -- {:.1}s: {}",
                segment.start,
                segment.start + segment.duration,
                segment.dr.text,
            );
            trace!("{seek}: {segment:?}, in {:?}", start.elapsed());
            segments.push(segment)
        }
        Ok(segments)
    }
}
