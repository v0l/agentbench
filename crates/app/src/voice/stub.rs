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
    pub fn level(&self) -> f32 {
        0.0
    }
    #[doc(hidden)]
    pub fn show_recording(&mut self, _: f32, _: f32) {}
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
    pub fn cancel(&mut self) {}
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

pub fn local_device() -> String {
    "not in this build".into()
}
