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

pub use agent_voice::voice::models::{DEFAULT_MODEL, LOCAL_MODELS, model};

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub mode: Mode,
    /// The model to read speech with: one the agentbench server lists, or a `LOCAL_MODELS` id
    /// when it is read here.
    pub model: String,
    /// The server's base URL, ending in `/v1` for most.
    pub url: String,
    pub key: String,
    /// Sent as `model`. Servers that serve one model ignore it, OpenAI wants `whisper-1`.
    pub server_model: String,
    /// The voice replies are read aloud in, one of the server's voice names (`Hadley`). The
    /// agentbench server speaks them.
    pub reply_voice: String,
    pub hold_to_talk: bool,
    pub converse: bool,
    pub input_device: String,
    pub talk_key: String,
}

pub const DEFAULT_TALK_KEY: &str = "F9";

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
            reply_voice: agent_voice::voice::DEFAULT_SPOKEN_VOICE.into(),
            hold_to_talk: cfg!(target_os = "android"),
            converse: false,
            input_device: String::new(),
            talk_key: DEFAULT_TALK_KEY.into(),
        }
    }
}

impl Settings {
    /// The model this device's own Whisper reads with: the one picked, if it is one it has.
    pub fn local_model(&self) -> &str {
        if model(&self.model).is_some() { &self.model } else { DEFAULT_MODEL }
    }

    pub fn load(storage: Option<&dyn eframe::Storage>) -> Settings {
        let mut s = Settings::default();
        let Some(st) = storage else {
            return s;
        };
        match st.get_string("voice_mode").as_deref() {
            Some("local") if cfg!(local_speech) => s.mode = Mode::Local,
            Some("server") => s.mode = Mode::Server,
            Some("agentbench") => s.mode = Mode::Agentbench,
            _ => {}
        }
        // Kept as saved, even when this build does not know it: the server lists its own models
        // and reads with any of them. This device's Whisper falls back to the default instead.
        if let Some(m) = st.get_string("voice_model").filter(|m| !m.trim().is_empty()) {
            s.model = m;
        }
        if let Some(h) = st.get_string("voice_hold") {
            s.hold_to_talk = h == "1";
        }
        s.converse = st.get_string("voice_converse").as_deref() == Some("1");
        s.input_device = st.get_string("voice_input").unwrap_or_default();
        if let Some(k) = st.get_string("voice_talk_key").filter(|k| !k.trim().is_empty()) {
            s.talk_key = k;
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
        st.set_string("voice_hold", if self.hold_to_talk { "1" } else { "0" }.into());
        st.set_string("voice_converse", if self.converse { "1" } else { "0" }.into());
        st.set_string("voice_input", self.input_device.clone());
        st.set_string("voice_talk_key", self.talk_key.clone());
    }

    pub fn talk_key(&self) -> Option<eframe::egui::Key> {
        eframe::egui::Key::from_name(self.talk_key.trim())
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

    #[derive(Default)]
    struct Mem(std::collections::HashMap<String, String>);
    impl eframe::Storage for Mem {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    /// The server lists its own models and voices, and a phone must keep one it picked from that
    /// list even when its own build has never heard of it: a new model on the server must not
    /// need a new release of the app.
    #[test]
    fn a_model_and_voice_only_the_server_knows_are_kept() {
        let mut m = Mem::default();
        let picked = Settings {
            mode: Mode::Agentbench,
            model: "parakeet-tdt-0.6b".into(),
            reply_voice: "Alfie".into(),
            ..Default::default()
        };
        picked.save(&mut m);
        let back = Settings::load(Some(&m));
        assert_eq!(back.model, "parakeet-tdt-0.6b");
        assert_eq!(back.reply_voice, "Alfie");
    }

    #[test]
    fn hold_to_talk_defaults_to_android_only_and_is_kept() {
        assert_eq!(Settings::default().hold_to_talk, cfg!(target_os = "android"));
        let mut m = Mem::default();
        let flipped =
            Settings { hold_to_talk: !Settings::default().hold_to_talk, ..Default::default() };
        flipped.save(&mut m);
        assert_eq!(Settings::load(Some(&m)).hold_to_talk, flipped.hold_to_talk);
    }

    #[test]
    fn the_conversation_is_off_by_default_and_its_settings_are_kept() {
        let s = Settings::default();
        assert!(!s.converse);
        assert_eq!(s.talk_key(), Some(eframe::egui::Key::F9));
        let mut m = Mem::default();
        Settings {
            converse: true,
            input_device: "USB mic".into(),
            talk_key: "F8".into(),
            ..Default::default()
        }
        .save(&mut m);
        let back = Settings::load(Some(&m));
        assert!(back.converse);
        assert_eq!(back.input_device, "USB mic");
        assert_eq!(back.talk_key(), Some(eframe::egui::Key::F8));
    }

    #[test]
    fn a_server_without_an_address_is_a_problem() {
        let s = Settings { mode: Mode::Server, ..Default::default() };
        assert!(s.problem().is_some());
        let s = Settings { mode: Mode::Server, url: "http://x/v1".into(), ..Default::default() };
        assert!(s.problem().is_none());
    }
}
