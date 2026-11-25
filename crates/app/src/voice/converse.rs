use std::collections::VecDeque;

#[derive(Default)]
pub struct Converse {
    after: i64,
    reply: Option<i64>,
    cut: usize,
    cancelled: Option<i64>,
    queue: VecDeque<String>,
}

fn sentence_end(text: &str, from: usize) -> Option<usize> {
    let mut chars = text[from..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let at = from + i;
        if c == '\n' {
            return Some(at + 1);
        }
        if matches!(c, '.' | '!' | '?' | ':')
            && let Some(&(_, next)) = chars.peek()
            && next.is_whitespace()
        {
            return Some(at + c.len_utf8());
        }
    }
    None
}

impl Converse {
    pub fn begin(&mut self, last_seen: i64) {
        self.after = last_seen;
        self.reply = None;
        self.cut = 0;
        self.cancelled = None;
        self.queue.clear();
    }

    pub fn feed(&mut self, id: i64, text: &str, complete: bool) {
        if id <= self.after || self.cancelled == Some(id) {
            return;
        }
        if self.reply != Some(id) {
            self.reply = Some(id);
            self.cut = 0;
        }
        while let Some(end) = sentence_end(text, self.cut) {
            self.push(&text[self.cut..end]);
            self.cut = end;
        }
        if complete && self.cut < text.len() {
            self.push(&text[self.cut..]);
            self.cut = text.len();
        }
    }

    fn push(&mut self, sentence: &str) {
        let said = agent_voice::voice::audio::speakable(sentence);
        if !said.is_empty() {
            self.queue.push_back(said);
        }
    }

    pub fn next(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// How many sentences are waiting to be spoken.
    pub fn waiting(&self) -> usize {
        self.queue.len()
    }

    pub fn barge_in(&mut self) {
        self.queue.clear();
        self.cancelled = self.reply;
        if let Some(id) = self.reply {
            self.after = self.after.max(id - 1);
        }
    }
}

/// What the talk key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Talk {
    /// Open the microphone.
    Start,
    /// Close it and send what was heard.
    Finish,
    Nothing,
}

/// The talk key's own state, so a press and a release (hold to talk) and a press on its own
/// (toggle) both drive the microphone the way the setting says.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TalkKey {
    down: bool,
}

impl TalkKey {
    /// A press of the key. In hold to talk mode the key is held until the release; otherwise the
    /// press is the whole gesture and the key is up again at once.
    pub fn press(&mut self, hold_to_talk: bool, listening: bool) -> Talk {
        if self.down {
            return Talk::Nothing;
        }
        self.down = hold_to_talk;
        if listening { Talk::Finish } else { Talk::Start }
    }

    /// A release of the key. Only hold to talk mode acts on it.
    pub fn release(&mut self, hold_to_talk: bool, listening: bool) -> Talk {
        self.down = false;
        if hold_to_talk && listening { Talk::Finish } else { Talk::Nothing }
    }

    pub fn down(&self) -> bool {
        self.down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_talk_key_holds_to_talk_and_toggles() {
        let mut k = TalkKey::default();
        assert_eq!(k.press(true, false), Talk::Start);
        assert_eq!(k.press(true, false), Talk::Nothing, "a key already down does nothing");
        assert_eq!(k.release(true, true), Talk::Finish, "hold to talk ends on the release");
        assert_eq!(k.release(true, false), Talk::Nothing, "a release with no take does nothing");

        let mut k = TalkKey::default();
        assert_eq!(k.press(false, false), Talk::Start);
        assert!(!k.down(), "a tap in toggle mode does not hold the key");
        assert_eq!(k.release(false, true), Talk::Nothing, "and its release does not end the take");
        assert_eq!(k.press(false, true), Talk::Finish, "the next press ends it");
        assert_eq!(k.press(false, false), Talk::Start);
        assert_eq!(k.release(false, true), Talk::Nothing);
        assert_eq!(k.press(false, true), Talk::Finish);
    }

    #[test]
    #[test]
    fn sentences_are_spoken_as_they_complete() {
        let mut c = Converse::default();
        c.feed(5, "I started two workers.", false);
        assert!(!c.pending());
        c.feed(5, "I started two workers. They are", false);
        assert_eq!(c.next().as_deref(), Some("I started two workers."));
        assert_eq!(c.next(), None);
        c.feed(5, "I started two workers. They are busy now", true);
        assert_eq!(c.next().as_deref(), Some("They are busy now."));
    }

    #[test]
    fn a_barge_in_cancels_the_rest_of_the_reply() {
        let mut c = Converse::default();
        c.feed(7, "One. Two. Three", false);
        assert_eq!(c.next().as_deref(), Some("One."));
        c.barge_in();
        assert!(!c.pending());
        c.feed(7, "One. Two. Three. Four.", true);
        assert!(!c.pending());
        assert_eq!(c.next(), None);
        c.feed(8, "A new answer.", true);
        assert_eq!(c.next().as_deref(), Some("A new answer."));
    }

    #[test]
    fn replies_already_seen_when_the_loop_began_stay_silent() {
        let mut c = Converse::default();
        c.begin(10);
        c.feed(9, "Old news.", true);
        assert!(!c.pending());
        c.feed(11, "Fresh news.", true);
        assert!(c.pending());
    }
}
