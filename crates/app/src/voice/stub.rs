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
    pub fn device(&self) -> &str {
        ""
    }
    pub fn start(&mut self, _: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
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
