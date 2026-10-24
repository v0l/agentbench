//! Where speech is read and with what. Kept on this device with the rest of the window's
//! settings, since the microphone is on this device.

/// Where the words come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Whisper on this machine, through candle.
    Local,
    /// An OpenAI compatible `/v1/audio/transcriptions`.
    Server,
    /// The agentbench server this window is connected to, which reads speech with Whisper on its own
    /// machine (on its GPU when it was built with CUDA). Needs no address or key of its own.
    Agentbench,
}

pub use egui_bench::voice::models::{DEFAULT_MODEL, LOCAL_MODELS, model};

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
    /// The voice replies are read aloud in, one of the server's voice names (`Hadley`). The
    /// agentbench server speaks them.
    pub reply_voice: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: if !cfg!(local_speech) {
                // A phone is better at listening than at reading: let the server do that.
                Mode::Agentbench
            } else {
                Mode::Local
            },
            model: DEFAULT_MODEL.into(),
            url: String::new(),
            key: String::new(),
            server_model: String::new(),
            reply_voice: egui_bench::voice::DEFAULT_SPOKEN_VOICE.into(),
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
            Some("local") if cfg!(local_speech) => s.mode = Mode::Local,
            Some("server") => s.mode = Mode::Server,
            Some("agentbench" | "foreman") => s.mode = Mode::Agentbench,
            _ => {}
        }
        if let Some(m) = st.get_string("voice_model").filter(|m| model(m).is_some()) {
            s.model = m;
        }
        s.url = st.get_string("voice_url").unwrap_or_default();
        s.key = st.get_string("voice_key").unwrap_or_default();
        s.server_model = st.get_string("voice_server_model").unwrap_or_default();
        // A Kokoro id (`af_heart`) from before Phonon is not a name the server knows; it would be
        // read in the default voice anyway, so it is dropped here.
        if let Some(v) = st.get_string("voice_reply").filter(|v| !v.is_empty() && !v.contains('_'))
        {
            s.reply_voice = v;
        }
        s
    }

    pub fn save(&self, st: &mut dyn eframe::Storage) {
        st.set_string(
            "voice_mode",
            match self.mode {
                Mode::Local => "local",
                Mode::Server => "server",
                Mode::Agentbench => "agentbench",
            }
            .into(),
        );
        st.set_string("voice_model", self.model.clone());
        st.set_string("voice_url", self.url.clone());
        st.set_string("voice_key", self.key.clone());
        st.set_string("voice_server_model", self.server_model.clone());
        st.set_string("voice_reply", self.reply_voice.clone());
    }

    /// What is missing before speech can be read, or `None` when it can.
    pub fn problem(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Local if !cfg!(local_speech) => {
                Some("this build has no local speech model; choose a speech server in settings")
            }
            Mode::Server if self.url.trim().is_empty() => {
                Some("set a speech server address in settings first")
            }
            _ => None,
        }
    }

    /// These settings as a plain speech server, with `mode` Agentbench turned into the connected
    /// agentbench server's own endpoint, token and the model picked here.
    pub fn resolved(&self, agentbench: Option<(&str, &str)>) -> Settings {
        let mut s = self.clone();
        if let (Mode::Agentbench, Some((url, token))) = (self.mode, agentbench) {
            s.mode = Mode::Server;
            s.url = format!("{}/v1", url.trim_end_matches('/'));
            s.key = token.to_string();
            s.server_model = self.model.clone();
        }
        s
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
}
