//! Where speech is read and with what. Kept on this device with the rest of the window's
//! settings, since the microphone is on this device.

/// Where the words come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Whisper through the engine, run by the agentbench server on this machine.
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
    pub auto_send: bool,
    pub auto_send_secs: u32,
    pub converse: bool,
    pub hands_free: bool,
    pub headphones: Option<bool>,
    pub echo_cancel: bool,
    pub input_device: String,
    pub talk_key: String,
    pub hang_secs: f32,
    pub min_speech_secs: f32,
    pub short_take_secs: f32,
    pub duplex_url: String,
    pub duplex_model: String,
    pub duplex_key: String,
    pub duplex_voice: String,
    pub duplex_replies: bool,
    pub duplex_free: bool,
    pub duplex_haru_only: bool,
    pub duplex_speak_first: bool,
    pub duplex_local: bool,
    pub duplex_path: String,
    pub duplex_gpu: String,
    pub engine_idle_secs: u64,
    pub engine_program: String,
}

pub const LOCAL_DUPLEX: &str = "Haru-4B";
pub const LOCAL: bool = cfg!(not(any(target_os = "android", target_arch = "wasm32")));
pub const DEFAULT_ENGINE_IDLE_SECS: u64 = 600;
const HF_REPO: &str = "models--higurashi-ai--Haru-4B";

pub fn default_model_dir_in(home: &std::path::Path, hf_home: Option<&std::path::Path>) -> std::path::PathBuf {
    let hub = match hf_home {
        Some(h) => h.join("hub"),
        None => home.join(".cache/huggingface/hub"),
    };
    let snapshots = hub.join(HF_REPO).join("snapshots");
    let newest = std::fs::read_dir(&snapshots)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("config.json").is_file())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    newest.unwrap_or_else(|| home.join("models").join(LOCAL_DUPLEX))
}

pub fn default_model_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let hf = std::env::var_os("HF_HOME").map(std::path::PathBuf::from);
    default_model_dir_in(&home, hf.as_deref())
}

pub fn model_problem(dir: &std::path::Path) -> Option<String> {
    agent_voice::engine::folder_problem(dir, LOCAL_DUPLEX)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DuplexModel {
    pub url: String,
    pub model: String,
    pub key: String,
    pub voice: String,
    pub speak_first: bool,
    pub free: bool,
}

pub const DEFAULT_TALK_KEY: &str = "F9";
pub const DEFAULT_AUTO_SEND_SECS: u32 = 3;
pub const AUTO_SEND_SECS: std::ops::RangeInclusive<u32> = 1..=30;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: if !LOCAL {
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
            auto_send: cfg!(target_os = "android"),
            auto_send_secs: DEFAULT_AUTO_SEND_SECS,
            converse: false,
            hands_free: false,
            headphones: None,
            echo_cancel: false,
            input_device: String::new(),
            talk_key: DEFAULT_TALK_KEY.into(),
            hang_secs: super::HANG_SECONDS,
            min_speech_secs: super::MIN_SPEECH_SECONDS,
            short_take_secs: super::SHORT_TAKE_SECONDS,
            duplex_url: String::new(),
            duplex_model: String::new(),
            duplex_key: String::new(),
            duplex_voice: String::new(),
            duplex_replies: false,
            duplex_free: true,
            duplex_haru_only: true,
            duplex_speak_first: false,
            duplex_local: false,
            duplex_path: String::new(),
            duplex_gpu: String::new(),
            engine_idle_secs: DEFAULT_ENGINE_IDLE_SECS,
            engine_program: String::new(),
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
            Some("local") if LOCAL => s.mode = Mode::Local,
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
        if let Some(a) = st.get_string("voice_auto_send") {
            s.auto_send = a == "1";
        }
        if let Some(n) = st.get_string("voice_auto_send_secs").and_then(|n| n.parse::<u32>().ok()) {
            s.auto_send_secs = n.clamp(*AUTO_SEND_SECS.start(), *AUTO_SEND_SECS.end());
        }
        s.converse = st.get_string("voice_converse").as_deref() == Some("1");
        s.hands_free = st.get_string("voice_hands_free").as_deref() == Some("1");
        s.headphones = match st.get_string("voice_headphones").as_deref() {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        s.echo_cancel = st.get_string("voice_echo_cancel").as_deref() == Some("1");
        s.input_device = st.get_string("voice_input").unwrap_or_default();
        if let Some(k) = st.get_string("voice_talk_key").filter(|k| !k.trim().is_empty()) {
            s.talk_key = k;
        }
        if let Some(h) = st.get_string("voice_hang_secs").and_then(|h| h.parse::<f32>().ok()) {
            s.hang_secs = h.clamp(*super::HANG_RANGE.start(), *super::HANG_RANGE.end());
        }
        if let Some(v) = st.get_string("voice_min_speech_secs").and_then(|v| v.parse::<f32>().ok()) {
            s.min_speech_secs = v.clamp(*super::MIN_SPEECH_RANGE.start(), *super::MIN_SPEECH_RANGE.end());
        }
        if let Some(v) = st.get_string("voice_short_take_secs").and_then(|v| v.parse::<f32>().ok()) {
            s.short_take_secs = v.clamp(*super::SHORT_TAKE_RANGE.start(), *super::SHORT_TAKE_RANGE.end());
        }
        s.url = st.get_string("voice_url").unwrap_or_default();
        s.key = st.get_string("voice_key").unwrap_or_default();
        s.server_model = st.get_string("voice_server_model").unwrap_or_default();
        s.duplex_url = st.get_string("voice_duplex_url").unwrap_or_default();
        s.duplex_model = st.get_string("voice_duplex_model").unwrap_or_default();
        s.duplex_key = st.get_string("voice_duplex_key").unwrap_or_default();
        s.duplex_voice = st.get_string("voice_duplex_voice").unwrap_or_default();
        s.duplex_replies = st.get_string("voice_duplex_says").as_deref() == Some("1");
        s.duplex_free = st.get_string("voice_duplex_free").as_deref() != Some("0");
        s.duplex_haru_only = st.get_string("voice_duplex_haru_only").as_deref() != Some("0");
        s.duplex_speak_first = st.get_string("voice_duplex_speak_first").as_deref() == Some("1");
        s.duplex_local = st.get_string("voice_duplex_choice").as_deref() == Some("haru");
        s.duplex_path = st.get_string("voice_duplex_path").unwrap_or_default();
        s.duplex_gpu = st.get_string("voice_duplex_gpu").unwrap_or_default();
        if let Some(n) = st.get_string("voice_engine_idle_secs").and_then(|n| n.parse().ok()) {
            s.engine_idle_secs = n;
        }
        s.engine_program = st.get_string("voice_engine_program").unwrap_or_default();
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
        st.set_string("voice_auto_send", if self.auto_send { "1" } else { "0" }.into());
        st.set_string("voice_auto_send_secs", self.auto_send_secs.to_string());
        st.set_string("voice_converse", if self.converse { "1" } else { "0" }.into());
        st.set_string("voice_hands_free", if self.hands_free { "1" } else { "0" }.into());
        match self.headphones {
            Some(on) => st.set_string("voice_headphones", if on { "1" } else { "0" }.into()),
            None => st.remove_string("voice_headphones"),
        }
        st.set_string("voice_echo_cancel", if self.echo_cancel { "1" } else { "0" }.into());
        st.set_string("voice_input", self.input_device.clone());
        st.set_string("voice_talk_key", self.talk_key.clone());
        st.set_string("voice_hang_secs", self.hang_secs.to_string());
        st.set_string("voice_min_speech_secs", self.min_speech_secs.to_string());
        st.set_string("voice_short_take_secs", self.short_take_secs.to_string());
        st.set_string("voice_duplex_url", self.duplex_url.clone());
        st.set_string("voice_duplex_model", self.duplex_model.clone());
        st.set_string("voice_duplex_key", self.duplex_key.clone());
        st.set_string("voice_duplex_voice", self.duplex_voice.clone());
        st.set_string("voice_duplex_says", if self.duplex_replies { "1" } else { "0" }.into());
        st.set_string("voice_duplex_free", if self.duplex_free { "1" } else { "0" }.into());
        st.set_string("voice_duplex_haru_only", if self.duplex_haru_only { "1" } else { "0" }.into());
        st.set_string("voice_duplex_speak_first", if self.duplex_speak_first { "1" } else { "0" }.into());
        st.set_string("voice_duplex_choice", if self.duplex_local { "haru" } else { "" }.into());
        st.set_string("voice_duplex_path", self.duplex_path.clone());
        st.set_string("voice_duplex_gpu", self.duplex_gpu.clone());
        st.set_string("voice_engine_idle_secs", self.engine_idle_secs.to_string());
        st.set_string("voice_engine_program", self.engine_program.clone());
    }

    pub fn duplex_dir(&self) -> std::path::PathBuf {
        match self.duplex_path.trim() {
            "" => default_model_dir(),
            p => std::path::PathBuf::from(p),
        }
    }

    pub fn local_duplex(&self) -> Option<std::path::PathBuf> {
        (self.duplex_local && self.duplex_url.trim().is_empty() && !self.duplex_needs_clip()).then(|| self.duplex_dir())
    }

    pub fn duplex_needs_clip(&self) -> bool {
        (self.duplex_local || !self.duplex_url.trim().is_empty()) && self.duplex_voice.trim().is_empty()
    }

    pub fn haru_only(&self) -> bool {
        self.duplex_haru_only && (self.duplex_local || !self.duplex_url.trim().is_empty())
    }

    pub fn duplex_gpu(&self) -> Option<u32> {
        self.duplex_gpu.trim().parse().ok()
    }

    pub fn duplex(&self) -> Option<DuplexModel> {
        let url = self.duplex_url.trim();
        (!url.is_empty() && !self.duplex_needs_clip()).then(|| DuplexModel {
            url: url.to_string(),
            model: self.duplex_model.trim().to_string(),
            key: self.duplex_key.trim().to_string(),
            voice: self.duplex_voice.trim().to_string(),
            speak_first: self.duplex_speak_first,
            free: self.duplex_free || self.duplex_haru_only,
        })
    }

    pub fn talk_key(&self) -> Option<eframe::egui::Key> {
        eframe::egui::Key::from_name(self.talk_key.trim())
    }

    /// What is missing before speech can be read, or `None` when it can.
    pub fn problem(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Local if !LOCAL => {
                Some("this device does not run speech models; choose the agentbench server in settings")
            }
            Mode::Server if self.url.trim().is_empty() => {
                Some("set a speech server address in settings first")
            }
            _ => None,
        }
    }

    /// These settings as a plain speech server: `Agentbench` becomes the connected server's own
    /// endpoint and token, and `Local` the agentbench server on this machine, which reads with the
    /// engine.
    pub fn resolved(&self, agentbench: Option<(&str, &str)>) -> Settings {
        self.resolved_with(agentbench, agentbench)
    }

    pub fn resolved_with(&self, agentbench: Option<(&str, &str)>, local: Option<(&str, &str)>) -> Settings {
        let mut s = self.clone();
        let to = match self.mode {
            Mode::Agentbench => agentbench,
            Mode::Local => local,
            Mode::Server => None,
        };
        if let Some((url, token)) = to {
            s.mode = Mode::Server;
            let url = url.trim().trim_end_matches('/');
            let url = if url.starts_with("http://") || url.starts_with("https://") {
                url.to_string()
            } else {
                format!("http://{url}")
            };
            s.url = format!("{}/v1", url.trim_end_matches('/'));
            s.key = token.to_string();
            s.server_model = if self.mode == Mode::Local { self.local_model().to_string() } else { self.model.clone() };
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

impl Settings {
    pub fn keys(&self) -> std::collections::BTreeMap<String, String> {
        let mut m = Keys::default();
        self.save(&mut m);
        m.0
    }

    pub fn from_keys(keys: std::collections::BTreeMap<String, String>) -> Settings {
        Settings::load(Some(&Keys(keys)))
    }

    pub fn merged(base: &std::collections::BTreeMap<String, String>, mine: &Settings, theirs: &std::collections::BTreeMap<String, String>) -> Settings {
        let mine = mine.keys();
        let mut out = mine.clone();
        for k in base.keys().chain(theirs.keys()).filter(|k| k.starts_with("voice_")) {
            if mine.get(k) != base.get(k) {
                continue;
            }
            match theirs.get(k) {
                Some(v) => out.insert(k.clone(), v.clone()),
                None => out.remove(k),
            };
        }
        Settings::from_keys(out)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn on_disk(path: &std::path::Path) -> Option<std::collections::BTreeMap<String, String>> {
        let text = std::fs::read_to_string(path).ok()?;
        let all: std::collections::HashMap<String, String> = ron::from_str(&text).ok()?;
        Some(Settings::from_keys(all.into_iter().filter(|(k, _)| k.starts_with("voice_")).collect()).keys())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Watch {
    path: std::path::PathBuf,
    base: std::collections::BTreeMap<String, String>,
    modified: Option<std::time::SystemTime>,
    checked: Option<web_time::Instant>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Watch {
    pub const EVERY: web_time::Duration = web_time::Duration::from_secs(2);

    pub fn new(path: std::path::PathBuf, current: &Settings) -> Watch {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let base = Settings::on_disk(&path).unwrap_or_else(|| current.keys());
        Watch { path, base, modified, checked: None }
    }

    pub fn absorb(&mut self, speech: &mut Settings, now: web_time::Instant, force: bool) -> bool {
        if !force && self.checked.is_some_and(|c| now.saturating_duration_since(c) < Self::EVERY) {
            return false;
        }
        self.checked = Some(now);
        let modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified == self.modified {
            return false;
        }
        let Some(theirs) = Settings::on_disk(&self.path) else { return false };
        self.modified = modified;
        if theirs == self.base {
            return false;
        }
        let merged = Settings::merged(&self.base, speech, &theirs);
        self.base = theirs;
        let changed = merged != *speech;
        *speech = merged;
        changed
    }

    pub fn saved(&mut self, speech: &Settings) {
        self.base = speech.keys();
    }
}

#[derive(Default)]
struct Keys(std::collections::BTreeMap<String, String>);

impl eframe::Storage for Keys {
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
    fn a_change_on_disk_is_kept_unless_this_window_changed_the_same_setting() {
        let base = Settings { duplex_local: true, hang_secs: 3.0, ..Default::default() };
        let mut theirs = base.keys();
        theirs.insert("voice_duplex_voice".into(), "/disk.wav".into());
        theirs.insert("voice_hang_secs".into(), "5".into());
        theirs.insert("voice_headphones".into(), "1".into());
        let mine = Settings { hang_secs: 8.0, duplex_replies: true, ..base.clone() };
        let out = Settings::merged(&base.keys(), &mine, &theirs);
        assert_eq!(out.duplex_voice, "/disk.wav", "untouched here, so the disk wins");
        assert_eq!(out.headphones, Some(true));
        assert_eq!(out.hang_secs, 8.0, "changed here, so this window wins");
        assert!(out.duplex_replies);
        assert!(out.duplex_local);
        let mut gone = base.keys();
        gone.insert("voice_headphones".into(), "0".into());
        let before = Settings { headphones: Some(false), ..base.clone() };
        let out = Settings::merged(&gone, &before, &base.keys());
        assert_eq!(out.headphones, None, "a setting removed on disk is removed here");
    }

    #[test]
    fn the_voice_settings_are_read_from_an_eframe_ron_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.ron");
        let mut all = std::collections::HashMap::new();
        all.insert("voice_duplex_voice".to_string(), "/me.wav".to_string());
        all.insert("tiles".to_string(), "1,2".to_string());
        std::fs::write(&path, ron::ser::to_string_pretty(&all, Default::default()).unwrap()).unwrap();
        let keys = Settings::on_disk(&path).unwrap();
        assert!(keys.keys().all(|k| k.starts_with("voice_")));
        assert_eq!(keys["voice_duplex_voice"], "/me.wav");
        assert_eq!(keys["voice_hang_secs"], "3", "a missing setting reads as its default");
        assert_eq!(Settings::on_disk(&dir.path().join("none.ron")), None);
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
    fn auto_send_is_on_for_android_only_with_a_delay_that_is_kept_and_clamped() {
        use eframe::Storage;
        let d = Settings::default();
        assert_eq!(d.auto_send, cfg!(target_os = "android"));
        assert_eq!(d.auto_send_secs, 3);
        let mut m = Mem::default();
        Settings { auto_send: !d.auto_send, auto_send_secs: 7, ..Default::default() }.save(&mut m);
        let back = Settings::load(Some(&m));
        assert_eq!(back.auto_send, !d.auto_send);
        assert_eq!(back.auto_send_secs, 7);
        m.set_string("voice_auto_send_secs", "900".into());
        assert_eq!(Settings::load(Some(&m)).auto_send_secs, 30);
        m.set_string("voice_auto_send_secs", "0".into());
        assert_eq!(Settings::load(Some(&m)).auto_send_secs, 1);
    }

    #[test]
    fn the_noise_settings_default_kept_and_clamped() {
        let d = Settings::default();
        assert_eq!(d.min_speech_secs, 0.25);
        assert_eq!(d.short_take_secs, 2.0);
        let mut m = Mem::default();
        Settings { min_speech_secs: 0.5, short_take_secs: 4.0, ..Default::default() }.save(&mut m);
        let back = Settings::load(Some(&m));
        assert_eq!((back.min_speech_secs, back.short_take_secs), (0.5, 4.0));
        eframe::Storage::set_string(&mut m, "voice_min_speech_secs", "99".into());
        eframe::Storage::set_string(&mut m, "voice_short_take_secs", "-1".into());
        let back = Settings::load(Some(&m));
        assert_eq!((back.min_speech_secs, back.short_take_secs), (2.0, 0.0));
    }

    #[test]
    fn the_hang_defaults_to_three_seconds_is_kept_and_clamped() {
        assert_eq!(Settings::default().hang_secs, 3.0);
        let mut m = Mem::default();
        Settings { hang_secs: 8.0, ..Default::default() }.save(&mut m);
        assert_eq!(Settings::load(Some(&m)).hang_secs, 8.0);
        eframe::Storage::set_string(&mut m, "voice_hang_secs", "99".into());
        assert_eq!(Settings::load(Some(&m)).hang_secs, 15.0);
        eframe::Storage::set_string(&mut m, "voice_hang_secs", "0".into());
        assert_eq!(Settings::load(Some(&m)).hang_secs, 1.0);
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
    fn hands_free_is_off_by_default_and_the_headphones_answer_is_kept() {
        let s = Settings::default();
        assert!(!s.hands_free);
        assert_eq!(s.headphones, None);
        assert!(!s.echo_cancel);
        let mut m = Mem::default();
        Settings { hands_free: true, headphones: Some(false), echo_cancel: true, ..Default::default() }
            .save(&mut m);
        let back = Settings::load(Some(&m));
        assert!(back.hands_free);
        assert_eq!(back.headphones, Some(false));
        assert!(back.echo_cancel);
        Settings { headphones: None, ..back }.save(&mut m);
        assert_eq!(Settings::load(Some(&m)).headphones, None);
    }

    #[test]
    fn the_duplex_model_is_off_until_a_server_is_named_and_is_kept() {
        let s = Settings::default();
        assert_eq!(s.duplex(), None);
        assert!(!s.duplex_replies, "Haru reading queued text is hard to follow, so replies stay text to speech");
        assert!(!s.duplex_speak_first);
        let mut m = Mem::default();
        Settings {
            duplex_url: " http://gpu:8080 ".into(),
            duplex_model: "Haru-4B".into(),
            duplex_key: "k".into(),
            duplex_voice: "/tmp/me.wav".into(),
            duplex_replies: true,
            duplex_speak_first: true,
            ..Default::default()
        }
        .save(&mut m);
        let back = Settings::load(Some(&m));
        assert!(back.duplex_replies);
        assert_eq!(
            back.duplex(),
            Some(DuplexModel {
                url: "http://gpu:8080".into(),
                model: "Haru-4B".into(),
                key: "k".into(),
                voice: "/tmp/me.wav".into(),
                speak_first: true,
                free: true,
            })
        );
    }

    #[test]
    fn haru_replies_in_its_own_words_by_default_and_relay_is_kept() {
        use eframe::Storage;
        assert!(Settings::default().duplex_free);
        let mut m = Mem::default();
        assert!(Settings::load(Some(&m)).duplex_free, "a device that never saved it gets free mode");
        Settings { duplex_free: false, ..Default::default() }.save(&mut m);
        assert_eq!(m.get_string("voice_duplex_free").as_deref(), Some("0"));
        assert!(!Settings::load(Some(&m)).duplex_free);
        let s = Settings { duplex_url: "http://gpu:8080".into(), duplex_voice: "/me.wav".into(), duplex_free: false, duplex_haru_only: false, ..Default::default() };
        assert!(!s.duplex().unwrap().free);
    }

    #[test]
    fn haru_only_is_the_default_with_a_voice_model_and_a_saved_off_is_kept() {
        let mut m = Mem::default();
        assert!(Settings::load(Some(&m)).duplex_haru_only, "a device that never saved it gets Haru only");
        let legacy = Settings { duplex_local: true, ..Settings::load(Some(&m)) };
        assert!(legacy.haru_only());
        let off = Settings { duplex_haru_only: false, duplex_local: true, ..Default::default() };
        off.save(&mut m);
        let back = Settings::load(Some(&m));
        assert!(!back.duplex_haru_only && !back.haru_only());
        let on = Settings { duplex_haru_only: true, duplex_local: true, ..Default::default() };
        on.save(&mut m);
        assert!(Settings::load(Some(&m)).haru_only());
        let none = Settings { duplex_haru_only: true, ..Default::default() };
        assert!(!none.haru_only(), "no voice model, nothing to keep");
        let remote = Settings { duplex_url: "http://gpu:8080".into(), duplex_voice: "/me.wav".into(), duplex_free: false, duplex_haru_only: true, ..Default::default() };
        assert!(remote.duplex().unwrap().free, "Haru only always talks for itself");
    }

    #[test]
    fn replies_in_its_voice_saved_on_by_the_old_default_load_off() {
        use eframe::Storage;
        let mut m = Mem::default();
        m.set_string("voice_duplex_replies", "1".into());
        assert!(!Settings::load(Some(&m)).duplex_replies);
    }

    #[test]
    fn the_local_voice_model_is_off_by_default_and_its_settings_are_kept() {
        let s = Settings::default();
        assert!(!s.duplex_local);
        assert_eq!(s.engine_idle_secs, 600);
        assert_eq!(s.local_duplex(), None);
        let mut m = Mem::default();
        Settings {
            duplex_local: true,
            duplex_voice: "/m/me.wav".into(),
            duplex_path: "/m/Haru-4B".into(),
            duplex_gpu: "1".into(),
            engine_idle_secs: 0,
            engine_program: "/opt/ib/inference-bench-server".into(),
            ..Default::default()
        }
        .save(&mut m);
        let back = Settings::load(Some(&m));
        assert!(back.duplex_local);
        assert_eq!(back.duplex_gpu, "1");
        assert_eq!(back.engine_idle_secs, 0);
        assert_eq!(back.engine_program, "/opt/ib/inference-bench-server");
        assert_eq!(back.local_duplex(), Some(std::path::PathBuf::from("/m/Haru-4B")));
    }

    #[test]
    fn a_duplex_model_without_a_voice_clip_is_not_used() {
        let s = Settings { duplex_local: true, ..Default::default() };
        assert!(s.duplex_needs_clip());
        assert_eq!(s.local_duplex(), None, "an unconditioned voice is noise");
        let s = Settings { duplex_url: "http://gpu:8080".into(), ..Default::default() };
        assert!(s.duplex_needs_clip());
        assert_eq!(s.duplex(), None);
        let s = Settings { duplex_local: true, duplex_voice: "/tmp/me.wav".into(), ..Default::default() };
        assert!(!s.duplex_needs_clip());
        assert!(s.local_duplex().is_some());
        assert!(!Settings::default().duplex_needs_clip(), "none chosen needs nothing");
    }

    #[test]
    fn a_server_address_wins_over_the_local_model() {
        let s = Settings {
            duplex_local: true,
            duplex_url: "http://gpu:8080".into(),
            duplex_voice: "/tmp/me.wav".into(),
            ..Default::default()
        };
        assert_eq!(s.local_duplex(), None);
        assert!(s.duplex().is_some());
    }

    #[test]
    fn the_model_folder_defaults_to_the_hugging_face_cache_then_home_models() {
        let home = tempfile::tempdir().unwrap();
        let plain = home.path().join("models/Haru-4B");
        assert_eq!(default_model_dir_in(home.path(), None), plain);
        let snap = home.path().join(".cache/huggingface/hub/models--higurashi-ai--Haru-4B/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("config.json"), "{}").unwrap();
        assert_eq!(default_model_dir_in(home.path(), None), snap);
        let hf = home.path().join("hf");
        assert_eq!(default_model_dir_in(home.path(), Some(&hf)), plain, "HF_HOME replaces the cache");
    }

    #[test]
    fn a_model_folder_says_plainly_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = model_problem(dir.path()).unwrap();
        assert!(p.contains("config.json") && p.contains("model.safetensors"), "{p}");
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        let p = model_problem(dir.path()).unwrap();
        assert!(!p.contains("config.json") && p.contains("model.safetensors"), "{p}");
        std::fs::write(dir.path().join("model.safetensors"), "").unwrap();
        assert_eq!(model_problem(dir.path()), None);
        let gone = model_problem(&dir.path().join("nope")).unwrap();
        assert!(gone.contains("no folder"), "{gone}");
    }

    #[test]
    fn this_device_reads_with_its_own_agentbench_server_and_a_model_it_knows() {
        let s = Settings { mode: Mode::Local, model: "whisper-small.en".into(), ..Default::default() };
        let r = s.resolved_with(Some(("http://far:7420", "remote")), Some(("127.0.0.1:7420", "mine")));
        assert_eq!((r.mode, r.endpoint().as_str(), r.key.as_str(), r.server_model.as_str()), (Mode::Server, "http://127.0.0.1:7420/v1/audio/transcriptions", "mine", "whisper-small.en"));
        let odd = Settings { model: "large-from-another-server".into(), ..s.clone() };
        assert_eq!(odd.resolved_with(None, Some(("127.0.0.1:7420", "mine"))).server_model, DEFAULT_MODEL);
        assert_eq!(s.resolved_with(Some(("http://far:7420", "remote")), None).mode, Mode::Local, "no server here yet");
        assert_eq!(s.problem(), None, "a desktop build reads here with no feature");
    }

    #[test]
    fn a_server_address_without_a_scheme_still_reaches_the_agentbench_endpoint() {
        let s = Settings { mode: Mode::Agentbench, ..Default::default() };
        let r = s.resolved(Some(("127.0.0.1:7420", "t")));
        assert_eq!(r.endpoint(), "http://127.0.0.1:7420/v1/audio/transcriptions");
        let r = s.resolved(Some(("http://box:1/", "t")));
        assert_eq!(r.endpoint(), "http://box:1/v1/audio/transcriptions");
    }

    #[test]
    fn a_server_without_an_address_is_a_problem() {
        let s = Settings { mode: Mode::Server, ..Default::default() };
        assert!(s.problem().is_some());
        let s = Settings { mode: Mode::Server, url: "http://x/v1".into(), ..Default::default() };
        assert!(s.problem().is_none());
    }
}
