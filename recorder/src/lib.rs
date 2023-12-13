#![feature(iter_intersperse)]

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

const SAMPLERATE: usize = 48_000;
const N_CHANNELS: usize = 1;

fn format_filename(sequence: usize) -> String {
    format!("{sequence:0>8}.wav")
}

pub struct Recorder {
    base: PathBuf,
}

impl Recorder {
    pub async fn new(base_dir: PathBuf) -> Self {
        if !tokio::fs::try_exists(&base_dir).await.unwrap() {
            tokio::fs::create_dir(&base_dir).await.unwrap();
        }
        Self { base: base_dir }
    }

    pub async fn session(&self, id: PathBuf) -> Session {
        debug!(?id, "new session");
        let folder = self.base.join(id);
        if !tokio::fs::try_exists(&folder).await.unwrap() {
            tokio::fs::create_dir(&folder).await.unwrap();
        }
        Session { base: folder }
    }
}

pub struct Session {
    base: PathBuf,
}

impl Session {
    pub fn new_speaker(&self, id: PathBuf) -> Speaker {
        Speaker {
            base: self.base.join(id),
            sequence: 0,
            buffer: Vec::with_capacity(SAMPLERATE * 60),
        }
    }
}

pub struct Speaker {
    base: PathBuf,
    sequence: usize,
    buffer: Vec<i16>,
}

impl Speaker {
    pub async fn listen(mut self) -> Sink {
        let (send, mut recv) = tokio::sync::mpsc::channel::<Audio>(256);
        let base = self.base.clone();
        if !tokio::fs::try_exists(&base).await.unwrap() {
            tokio::fs::create_dir(&base).await.unwrap();
        }

        let task = tokio::task::spawn(async move {
            debug!(base=?self.base, "started recording");
            while let Some(audio) = recv.recv().await {
                match audio {
                    Audio::Packets(mut samples) => {
                        self.buffer.append(&mut samples);
                    }
                    Audio::Silence => {
                        // TODO: make this more aware of how long silence usually is, as it might over do?
                        // currently 20ms of SAMPLERATE
                        self.buffer.append(&mut vec![0; SAMPLERATE / 50])
                    }
                };
                if self.buffer.len() >= SAMPLERATE * 60 {
                    let samples = self.buffer.drain(..).collect::<Vec<_>>();
                    self.save_chunk(self.sequence, samples)
                        .await
                        .expect("failed to save audio");
                    self.sequence += 1;
                }
            }
            debug!(base=?self.base, "finished recording");

            self
        });

        Sink {
            sender: send,
            task,
            base,
        }
    }

    async fn save_chunk(&self, seq: usize, samples: Vec<i16>) -> Result<PathBuf, std::io::Error> {
        let path = self.base.join(format_filename(seq));
        debug!(?seq, ?path, "saving chunk");
        let mut file = tokio::fs::File::create(&path).await?;

        let mut buf = std::io::Cursor::new(Vec::with_capacity(
            samples.len() * std::mem::size_of::<i16>(),
        ));

        // TODO: potentially blocking, though entirely in-memory. maybe put in `spawn_blocking`?
        wav::write(
            wav::Header::new(
                wav::WAV_FORMAT_PCM,
                N_CHANNELS as u16,
                SAMPLERATE as u32,
                16,
            ),
            &wav::BitDepth::Sixteen(samples),
            &mut buf,
        )?;

        file.write_all(&buf.into_inner()).await?;

        Ok(path)
    }
}

pub struct Sink {
    sender: tokio::sync::mpsc::Sender<Audio>,
    task: tokio::task::JoinHandle<Speaker>,
    base: PathBuf,
}

impl Sink {
    pub async fn send_audio(
        &self,
        audio: Audio,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<Audio>> {
        self.sender.send(audio).await
    }

    pub async fn do_merge(self) {
        debug!(base=?self.base, "merging");
        drop(self.sender);

        let mut speaker = self.task.await.expect("recorder thread paniced"); // TODO: make this whole function failable?
        let buffer = std::mem::take(&mut speaker.buffer);

        speaker
            .save_chunk(speaker.sequence, buffer)
            .await
            .expect("failed to save extra samples");

        let files = (0..=speaker.sequence)
            .map(|i| {
                format!(
                    "file '{}'",
                    self.base
                        .join(format_filename(i))
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                )
            })
            .intersperse("\n".to_string())
            .collect::<String>();

        let input = self.base.join("files.txt");
        tokio::fs::write(&input, files)
            .await
            .expect("failed to write file list");

        let command = tokio::process::Command::new("ffmpeg")
            .arg("-nostdin")
            .args(["-f", "concat"])
            .args(["-safe", "0"])
            .args([
                "-i",
                input.canonicalize().unwrap().to_string_lossy().as_ref(),
            ])
            .args(["-compression_level", "12"])
            .arg(self.base.join("combined.flac"))
            .spawn()
            .unwrap();

        let result = command
            .wait_with_output()
            .await
            .expect("ffmpeg command failed");

        if !result.status.success() {
            warn!(?result, "merge failed");
        } else {
            info!("merged files");
        }
    }
}

pub enum Audio {
    Packets(Vec<i16>),
    Silence,
}
