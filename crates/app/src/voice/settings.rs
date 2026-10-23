//! Where speech is read and with what. Kept on this device with the rest of the window's
//! settings, since the microphone is on this device.

/// Where the words come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Whisper on this machine, through candle.
    Local,
    /// An OpenAI compatible `/v1/audio/transcriptions`.
    Server,
}

/// The models Whisper can be run from here: id, Hugging Face repository, label, download size.
///
/// English only and small. They are the ones that read a microphone well on a CPU in a second or
/// two; anything bigger or multilingual is a job for a server.
pub const LOCAL_MODELS: &[(&str, &str, &str, u32)] = &[
    ("whisper-tiny.en", "openai/whisper-tiny.en", "tiny, English (fastest)", 151),
    ("whisper-base.en", "openai/whisper-base.en", "base, English", 290),
    ("whisper-small.en", "openai/whisper-small.en", "small, English (most accurate)", 967),
];

/// The model used when none is chosen.
pub const DEFAULT_MODEL: &str = "whisper-base.en";

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub mode: Mode,
    /// A `LOCAL_MODELS` id.
    pub model: String,
    /// The server's base URL, ending in `/v1` for most.
    pub url: String,
    pub key: String,
    /// Sent as `model`. Servers that serve one model ignore it, OpenAI wants `whisper-1`.
    pub server_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: if cfg!(feature = "local-speech") && cfg!(not(target_arch = "wasm32")) {
                Mode::Local
            } else {
                Mode::Server
            },
            model: DEFAULT_MODEL.into(),
            url: String::new(),
            key: String::new(),
            server_model: String::new(),
        }
    }
}

impl Settings {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Settings {
        let mut s = Settings::default();
        let Some(st) = storage else {
            return s;
        };
        match st.get_string("voice_mode").as_deref() {
            Some("local") if cfg!(feature = "local-speech") => s.mode = Mode::Local,
            Some("server") => s.mode = Mode::Server,
            _ => {}
        }
        if let Some(m) = st.get_string("voice_model").filter(|m| model(m).is_some()) {
            s.model = m;
        }
        s.url = st.get_string("voice_url").unwrap_or_default();
        s.key = st.get_string("voice_key").unwrap_or_default();
        s.server_model = st.get_string("voice_server_model").unwrap_or_default();
        s
    }

    pub fn save(&self, st: &mut dyn eframe::Storage) {
        st.set_string(
            "voice_mode",
            match self.mode {
                Mode::Local => "local",
                Mode::Server => "server",
            }
            .into(),
        );
        st.set_string("voice_model", self.model.clone());
        st.set_string("voice_url", self.url.clone());
        st.set_string("voice_key", self.key.clone());
        st.set_string("voice_server_model", self.server_model.clone());
    }

    /// What is missing before speech can be read, or `None` when it can.
    pub fn problem(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Local if !cfg!(feature = "local-speech") => {
                Some("this build has no local speech model; choose a speech server in settings")
            }
            Mode::Server if self.url.trim().is_empty() => {
                Some("set a speech server address in settings first")
            }
            _ => None,
        }
    }

    /// The full URL of the transcription endpoint.
    pub fn endpoint(&self) -> String {
        let base = self.url.trim().trim_end_matches('/');
        if base.ends_with("/audio/transcriptions") {
            base.to_string()
        } else {
            format!("{base}/audio/transcriptions")
        }
    }
}

pub fn model(id: &str) -> Option<&'static (&'static str, &'static str, &'static str, u32)> {
    LOCAL_MODELS.iter().find(|m| m.0 == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_is_built_from_a_base_url() {
        let mut s = Settings { url: "http://localhost:8000/v1/".into(), ..Default::default() };
        assert_eq!(s.endpoint(), "http://localhost:8000/v1/audio/transcriptions");
        s.url = "http://h/v1/audio/transcriptions".into();
        assert_eq!(s.endpoint(), "http://h/v1/audio/transcriptions");
    }

    #[test]
    fn a_server_without_an_address_is_a_problem() {
        let s = Settings { mode: Mode::Server, ..Default::default() };
        assert!(s.problem().is_some());
        let s = Settings { mode: Mode::Server, url: "http://x/v1".into(), ..Default::default() };
        assert!(s.problem().is_none());
    }

    #[test]
    fn the_default_model_is_on_the_list() {
        assert!(model(DEFAULT_MODEL).is_some());
    }
}
