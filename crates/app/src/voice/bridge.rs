use super::duplex::Framer;
use super::link::{Event, Link, Options, ws_url};
use super::settings::DuplexModel;
use agent_voice::voice::audio::{DUPLEX_RATE, read_wav, resample, to_pcm16};
use base64::Engine;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const BARGE_IN: Duration = Duration::from_millis(400);
pub const QUEUE_QUIET: Duration = Duration::from_millis(1500);
pub const SAY_TIMEOUT: Duration = Duration::from_secs(20);
pub const VOICE_SECONDS: usize = 30;

pub const CARD_NAME: &str = "Bench";
pub const CARD_PERSONA: &str = "A calm, warm voice assistant who speaks in short plain sentences and never makes things up.";
pub const CARD_RELATIONSHIP: &str = "You relay answers from an assistant that manages the user's coding projects.";
pub const CARD_SITUATION: &str = "The user talks to the assistant hands free through you. Say the words you are given; otherwise listen and keep quiet.";

#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Interrupted,
    Heard(String),
    Notice(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    Off,
    Connecting,
    Ready,
    Reconnecting,
}

pub struct Bridge {
    link: Option<Link>,
    model: Option<DuplexModel>,
    framer: Framer,
    conn: Conn,
    said_at: Option<Instant>,
    voiced_at: Option<Instant>,
    speech_since: Option<Instant>,
    interrupted: bool,
    heard: Option<String>,
    manual: bool,
    pub sent_frames: usize,
    pub says: Vec<String>,
}

impl Default for Bridge {
    fn default() -> Self {
        Bridge {
            link: None,
            model: None,
            framer: Framer::new(super::RATE),
            conn: Conn::Off,
            said_at: None,
            voiced_at: None,
            speech_since: None,
            interrupted: false,
            heard: None,
            manual: false,
            sent_frames: 0,
            says: Vec::new(),
        }
    }
}

pub fn card() -> Value {
    json!({
        "name": CARD_NAME,
        "persona": CARD_PERSONA,
        "backchannel": "low",
        "relationship": CARD_RELATIONSHIP,
        "situation": CARD_SITUATION,
    })
}

pub fn voice_clip(path: &str) -> Result<Option<String>, String> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    let bytes = std::fs::read(path.trim()).map_err(|e| format!("the voice clip {path}: {e}"))?;
    let (pcm, rate) = read_wav(&bytes)?;
    let mut pcm = resample(&pcm, rate, DUPLEX_RATE);
    pcm.truncate(DUPLEX_RATE as usize * VOICE_SECONDS);
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(to_pcm16(&pcm))))
}

pub fn start_message(model: &DuplexModel) -> Result<Value, String> {
    let mut start = json!({"type": "start", "card": card(), "mode": "relay"});
    if !model.model.is_empty() {
        start["model"] = model.model.clone().into();
    }
    if let Some(voice) = voice_clip(&model.voice)? {
        start["voice"] = voice.into();
    }
    Ok(start)
}

pub fn heard_note(heard: &str, said: &str) -> String {
    if heard.trim().is_empty() {
        return format!("(I cut your spoken reply off before any of it was said.) {said}");
    }
    format!("(I cut your spoken reply off; I heard only: \"{}\") {said}", heard.trim())
}

impl Bridge {
    pub fn sync(
        &mut self,
        want: Option<&DuplexModel>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Option<String> {
        if want == self.model.as_ref() && (want.is_none() || self.link.is_some()) {
            return None;
        }
        self.close();
        let model = want?.clone();
        let start = match start_message(&model) {
            Ok(s) => s,
            Err(e) => return Some(e),
        };
        let options = Options { url: ws_url(&model.url), key: model.key.clone(), start };
        match Link::open(options, self.manual, wake) {
            Ok(link) => {
                self.link = Some(link);
                self.model = Some(model);
                self.conn = Conn::Connecting;
                None
            }
            Err(e) => Some(format!("the duplex model could not play: {e}")),
        }
    }

    pub fn close(&mut self) {
        let manual = self.manual;
        *self = Bridge { manual, ..Bridge::default() };
    }

    #[doc(hidden)]
    pub fn set_manual(&mut self) {
        self.manual = true;
    }

    pub fn active(&self) -> bool {
        self.link.is_some()
    }

    pub fn conn(&self) -> Conn {
        self.conn
    }

    pub fn speaking(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.speaking())
    }

    pub fn feed(&mut self, pcm: &[f32], rate: u32) {
        let Some(link) = &self.link else { return };
        if rate != super::RATE && self.sent_frames == 0 {
            self.framer = Framer::new(rate);
        }
        for frame in self.framer.push(pcm) {
            self.sent_frames += 1;
            link.send_frame(frame);
        }
    }

    pub fn say(&mut self, sentence: &str, now: Instant) {
        let Some(link) = &self.link else { return };
        link.say(sentence);
        self.says.push(sentence.to_string());
        self.said_at = Some(now);
        self.interrupted = false;
    }

    pub fn queued(&self, now: Instant) -> bool {
        let Some(said) = self.said_at else { return false };
        match self.voiced_at.filter(|v| *v >= said) {
            Some(v) => now.saturating_duration_since(v) < QUEUE_QUIET,
            None => now.saturating_duration_since(said) < SAY_TIMEOUT,
        }
    }

    pub fn tick(&mut self, user_speaking: bool, more_to_say: bool, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(link) = &self.link else { return out };
        for e in link.poll() {
            match e {
                Event::Ready => self.conn = Conn::Ready,
                Event::Lost { why, retry_in } => {
                    if self.conn == Conn::Ready {
                        out.push(Out::Notice(format!(
                            "the duplex model dropped ({why}); trying again in {:.1} s",
                            retry_in.as_secs_f32()
                        )));
                    }
                    self.conn = Conn::Reconnecting;
                    self.said_at = None;
                }
                Event::Refused(why) => out.push(Out::Notice(format!("the duplex model: {why}"))),
                Event::Said { text, .. } => out.push(Out::Heard(text)),
                Event::Text(_) => {}
            }
        }
        if link.speaking() {
            self.voiced_at = Some(now);
        }
        if !user_speaking {
            self.speech_since = None;
            return out;
        }
        let since = *self.speech_since.get_or_insert(now);
        if !self.interrupted
            && (self.queued(now) || more_to_say)
            && now.saturating_duration_since(since) >= BARGE_IN
        {
            link.interrupt();
            self.interrupted = true;
            self.said_at = None;
            out.push(Out::Interrupted);
        }
        out
    }

    pub fn note_heard(&mut self, heard: String) {
        self.heard = Some(heard);
    }

    pub fn take_heard(&mut self) -> Option<String> {
        self.heard.take()
    }

    #[doc(hidden)]
    pub fn pull_frame(&self) -> Option<Vec<f32>> {
        self.link.as_ref().filter(|l| l.manual()).map(|l| l.pull_frame())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_say_is_pending_until_the_model_has_spoken_it_and_gone_quiet() {
        let mut b = Bridge::default();
        let t = Instant::now();
        assert!(!b.queued(t));
        b.said_at = Some(t);
        assert!(b.queued(t + Duration::from_secs(5)), "the model may wait before it speaks");
        assert!(!b.queued(t + SAY_TIMEOUT), "but not for ever");
        b.voiced_at = Some(t + Duration::from_secs(1));
        assert!(b.queued(t + Duration::from_secs(2)));
        assert!(!b.queued(t + Duration::from_secs(3)));
    }

    #[test]
    fn the_start_names_the_model_and_the_fixed_card_in_relay_mode() {
        let m = DuplexModel { url: "http://x".into(), model: "Haru-4B".into(), key: String::new(), voice: String::new() };
        let s = start_message(&m).unwrap();
        assert_eq!(s["type"], "start");
        assert_eq!(s["mode"], "relay");
        assert_eq!(s["model"], "Haru-4B");
        assert!(s["card"]["relationship"].as_str().unwrap().contains("relay"));
        assert!(s.get("voice").is_none());
    }

    #[test]
    fn the_voice_clip_is_sent_at_24_khz_and_cut_to_thirty_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("me.wav");
        std::fs::write(&path, agent_voice::voice::audio::wav_bytes(&vec![0.25f32; 16_000 * 40], 16_000)).unwrap();
        let b64 = voice_clip(path.to_str().unwrap()).unwrap().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(bytes.len(), DUPLEX_RATE as usize * VOICE_SECONDS * 2);
        assert!(voice_clip("/no/such/clip.wav").unwrap_err().contains("clip"));
        assert_eq!(voice_clip("  ").unwrap(), None);
    }

    #[test]
    fn what_was_heard_before_a_barge_in_leads_the_next_turn() {
        let n = heard_note("I started two", "stop, what about the readme");
        assert!(n.contains("\"I started two\"") && n.ends_with("stop, what about the readme"));
        assert!(heard_note("", "hi").contains("before any of it"));
    }
}
