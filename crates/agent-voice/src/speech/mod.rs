//! The speech models themselves, run in this process: Whisper reads a take, and Phonon (or the
//! older Kokoro) says a sentence. Both go through candle, fetch their files from Hugging Face the first time, and stay
//! loaded after. Feature `speech`; add `speech-cuda` to run them on an NVIDIA card when there
//! is one.
//!
//! An app with an agent window uses these on the machine it runs on, or serves them to
//! other machines at `/v1/audio/transcriptions` and `/v1/audio/speech`, which is what
//! [`crate::voice::Server`] talks to.

mod download;
pub mod phonon;
pub mod speak;
mod speaker;
mod transcriber;
mod voicer;
mod whisper;

pub use crate::voice::{DEFAULT_MODEL, LOCAL_MODELS, ModelInfo, model};
pub use download::Progress;
pub use speaker::{MAX_SECONDS, Said, Speaker};
pub use transcriber::Transcriber;
pub use voicer::Voicer;
pub use whisper::{Files, Whisper, best_device};
