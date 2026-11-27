//! Platforms with no microphone to open here: the browser and Android.

use super::{Said, Settings};

#[derive(Default)]
pub struct Voice {
    pub status: String,
}

impl Voice {
    pub fn listening(&self) -> bool {
        false
    }
    pub fn reading(&self) -> bool {
        false
    }
    pub fn busy(&self) -> bool {
        false
    }
    pub fn seconds(&self) -> f32 {
        0.0
    }
    pub fn closing_in(&self, _: &Settings) -> Option<f32> {
        None
    }
    pub fn take_quiet_stop(&mut self) -> Option<f32> {
        None
    }
    pub fn take_speech_start(&mut self) -> bool {
        false
    }
    pub fn set_tap(&mut self, _: bool) {}
    pub fn take_tapped(&mut self) -> (Vec<f32>, u32) {
        (Vec::new(), super::RATE)
    }
    pub fn open(&self) -> bool {
        false
    }
    pub fn in_speech(&self) -> bool {
        false
    }
    pub fn start_open(&mut self, _: impl Fn() + Send + Sync + 'static, _: &str) -> Result<(), String> {
        Err("voice input is not available on this platform".into())
    }
    #[doc(hidden)]
    pub fn open_fake(&mut self) -> super::Mic {
        Default::default()
    }
    pub fn level(&self) -> f32 {
        0.0
    }
    #[doc(hidden)]
    pub fn show_recording(&mut self, _: f32, _: f32) {}
    #[doc(hidden)]
    pub fn stage_take(&mut self) {}
    pub fn staged(&self) -> bool {
        false
    }
    pub fn device(&self) -> &str {
        ""
    }
    pub fn start(&mut self, _: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
        Err("voice input is not available on this platform".into())
    }
    pub fn start_on(&mut self, _: impl Fn() + Send + Sync + 'static, _: &str) -> Result<(), String> {
        Err("voice input is not available on this platform".into())
    }
    pub fn read_take(
        &mut self,
        _: &[u8],
        _: &Settings,
        _: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), String> {
        Err("voice input is not available on this platform".into())
    }
    pub fn cancel(&mut self) {}
    pub fn take_raw(&mut self) -> Option<(Vec<f32>, u32)> {
        None
    }
    pub fn start_recording(&mut self, _: impl Fn() + Send + Sync + 'static, _: &str) -> Result<(), String> {
        Err("voice input is not available on this platform".into())
    }
    pub fn recording(&self) -> bool {
        false
    }
    #[doc(hidden)]
    pub fn record_fake(&mut self) -> super::Mic {
        Default::default()
    }
    pub fn finish(&mut self, _: &Settings, _: impl Fn() + Send + Sync + 'static) {}
    pub fn tick(&mut self, _: &Settings, _: impl Fn() + Send + Sync + Clone + 'static) {}
    pub fn poll(&mut self) -> Vec<Said> {
        Vec::new()
    }
    pub fn installed(_: &str) -> bool {
        false
    }
}

/// Where to get speech, and in which voice.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub url: String,
    pub token: String,
    pub voice: String,
}

/// Reading aloud needs a sound card to open, which the browser build does not do.
#[derive(Default)]
pub struct Reader;

impl Reader {
    pub fn busy(&self) -> bool {
        false
    }
    #[doc(hidden)]
    pub fn silent(&mut self) {}
    #[doc(hidden)]
    pub fn spoken(&mut self) -> Vec<String> {
        Vec::new()
    }
    pub fn stop(&mut self) {}
    pub fn start(
        &mut self,
        _: &str,
        _: Source,
        _: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), String> {
        Err("reading aloud is not available in the browser".into())
    }
    pub fn tick(&mut self) -> Vec<String> {
        Vec::new()
    }
}

pub fn input_devices() -> Vec<String> {
    Vec::new()
}

pub const VOICE_SECONDS: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    Off,
    Connecting,
    Ready,
    Reconnecting,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Interrupted,
    Heard(String),
    Notice(String),
    Words(String),
}

#[derive(Default)]
pub struct Bridge {
    pub sent_frames: usize,
    pub says: Vec<String>,
}

impl Bridge {
    pub fn sync(&mut self, _: Option<&super::DuplexModel>, _: impl Fn() + Send + Sync + 'static) -> Option<String> {
        None
    }
    pub fn close(&mut self) {}
    pub fn active(&self) -> bool {
        false
    }
    pub fn free(&self) -> bool {
        false
    }
    pub fn conn(&self) -> Conn {
        Conn::Off
    }
    pub fn speaking(&self) -> bool {
        false
    }
    pub fn feed(&mut self, _: &[f32], _: u32) {}
    pub fn say(&mut self, _: &str, _: web_time::Instant) {}
    pub fn queued(&self, _: web_time::Instant) -> bool {
        false
    }
    pub fn tick(&mut self, _: bool, _: bool, _: web_time::Instant) -> Vec<Out> {
        Vec::new()
    }
    pub fn note_heard(&mut self, _: String) {}
    pub fn take_heard(&mut self) -> Option<String> {
        None
    }
    #[doc(hidden)]
    pub fn set_manual(&mut self) {}
    #[doc(hidden)]
    pub fn pull_frame(&self) -> Option<Vec<f32>> {
        None
    }
    pub fn muted_frames(&self) -> u64 {
        0
    }
}

pub fn heard_note(heard: &str, said: &str) -> String {
    format!("(I heard only: \"{heard}\") {said}")
}

pub fn local_device() -> String {
    "not in this build".into()
}
