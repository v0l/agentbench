//! Whisper through candle, and the list of models it can be run from.
//!
//! One take of 16 kHz mono samples goes in and a string comes out. The window uses it for voice
//! input on this machine, the Android app uses it the same way, and the server uses it to read
//! speech for a window or a phone that has no model of its own.

mod whisper;
pub use whisper::{Files, Whisper, best_device};

pub use foreman_core::speech_models::{DEFAULT_MODEL, LOCAL_MODELS, ModelInfo, model};

mod transcriber;
pub use transcriber::{Progress, Transcriber};
