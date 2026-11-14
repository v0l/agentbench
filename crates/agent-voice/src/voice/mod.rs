//! Voice for an agent window: the helpers both directions share, and (with feature `agent`) a
//! client for a server that speaks OpenAI's audio endpoints. The models themselves are the
//! `speech` feature. The helpers need nothing and build for the browser too.

pub mod audio;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
pub mod models;

pub use audio::{SPEECH_RATE, chunks, read_wav, resample, speakable, to_whisper_rate, wav_bytes};
#[cfg(not(target_arch = "wasm32"))]
pub use client::Server;
pub use models::{
    DEFAULT_MODEL, DEFAULT_SPOKEN_VOICE, DEFAULT_VOICE, LOCAL_MODELS, ModelInfo, VOICES, Voice,
    model,
};
