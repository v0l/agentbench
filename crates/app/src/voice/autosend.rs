use super::settings::AUTO_SEND_SECS;

#[derive(Clone, PartialEq, Debug)]
pub struct Countdown {
    text: String,
    due: f64,
}

pub enum Tick {
    Waiting(u32),
    Fire,
    Cancel,
}

const STALE_AFTER: f64 = 5.0;

impl Countdown {
    pub fn start(text: &str, now: f64, secs: u32) -> Option<Countdown> {
        let secs = secs.clamp(*AUTO_SEND_SECS.start(), *AUTO_SEND_SECS.end());
        sendable(text).then(|| Countdown { text: text.to_string(), due: now + secs as f64 })
    }

    pub fn tick(&self, draft: &str, now: f64) -> Tick {
        if draft != self.text || now > self.due + STALE_AFTER {
            Tick::Cancel
        } else if now >= self.due {
            Tick::Fire
        } else {
            Tick::Waiting((self.due - now).ceil() as u32)
        }
    }
}

pub fn label(left: u32) -> String {
    format!("Sending in {left}, tap to cancel or edit")
}

pub fn sendable(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && !text.starts_with('/') && !is_filler(text)
}

pub fn is_filler(text: &str) -> bool {
    let t = text.trim();
    let bracketed = (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'));
    if bracketed {
        return true;
    }
    let plain: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    plain.is_empty()
        || matches!(
            plain.as_str(),
            "you"
                | "uh"
                | "um"
                | "hmm"
                | "mm"
                | "ah"
                | "oh"
                | "huh"
                | "okay"
                | "thank you"
                | "thanks"
                | "thanks for watching"
                | "bye"
                | "blank audio"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_countdown_waits_then_fires_once_the_delay_has_passed() {
        let c = Countdown::start("fix it", 10.0, 3).unwrap();
        assert!(matches!(c.tick("fix it", 10.0), Tick::Waiting(3)));
        assert!(matches!(c.tick("fix it", 12.2), Tick::Waiting(1)));
        assert!(matches!(c.tick("fix it", 13.0), Tick::Fire));
    }

    #[test]
    fn an_edited_or_stale_draft_cancels() {
        let c = Countdown::start("fix it", 0.0, 3).unwrap();
        assert!(matches!(c.tick("fix it now", 1.0), Tick::Cancel));
        assert!(matches!(c.tick("", 1.0), Tick::Cancel));
        assert!(matches!(c.tick("fix it", 60.0), Tick::Cancel));
    }

    #[test]
    fn nothing_worth_sending_starts_a_countdown() {
        for text in ["", "  \n", "/new", " /stop", "you", "Thank you.", "[BLANK_AUDIO]", "(silence)", "Um,", "..."] {
            assert!(Countdown::start(text, 0.0, 3).is_none(), "{text:?}");
        }
        assert!(Countdown::start("you should fix the parser", 0.0, 3).is_some());
    }

    #[test]
    fn the_delay_is_kept_within_its_range() {
        let c = Countdown::start("go", 0.0, 0).unwrap();
        assert!(matches!(c.tick("go", 0.0), Tick::Waiting(1)));
        let c = Countdown::start("go", 0.0, 500).unwrap();
        assert!(matches!(c.tick("go", 0.0), Tick::Waiting(30)));
    }
}
