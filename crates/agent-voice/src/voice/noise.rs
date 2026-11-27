pub const NO_SPEECH_THRESHOLD: f32 = 0.6;
pub const LOGPROB_THRESHOLD: f32 = -1.0;

pub fn not_speech(no_speech: f32, avg_logprob: f32) -> bool {
    no_speech > NO_SPEECH_THRESHOLD && avg_logprob < LOGPROB_THRESHOLD
}

pub fn sound_tag(text: &str) -> bool {
    let t = text.trim().trim_end_matches(['.', '!', '?', ',']);
    let pairs = [('*', '*'), ('[', ']'), ('(', ')'), ('<', '>'), ('\u{266a}', '\u{266a}')];
    let wrapped = t.chars().count() >= 2
        && pairs.iter().any(|&(open, close)| t.starts_with(open) && t.ends_with(close));
    wrapped || no_words(t)
}

pub fn no_words(text: &str) -> bool {
    !text.chars().any(char::is_alphanumeric)
}

pub fn silence_phrase(text: &str) -> bool {
    let plain: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        plain.as_str(),
        "you"
            | "thank you"
            | "thank you very much"
            | "thank you so much"
            | "thank you for watching"
            | "thanks"
            | "thanks for watching"
            | "thanks for watching bye"
            | "bye"
            | "bye bye"
            | "uh"
            | "um"
            | "hmm"
            | "mm"
            | "ah"
            | "huh"
            | "please subscribe"
            | "subscribe"
            | "blank audio"
            | "silence"
    ) || plain.starts_with("www ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whisper_calls_it_silence_only_when_both_defaults_agree() {
        assert!(not_speech(0.93, -1.9));
        assert!(!not_speech(0.93, -0.4), "a confident reading is kept even when no speech is likely");
        assert!(!not_speech(0.2, -1.9), "a doubtful reading of likely speech is kept");
        assert!(!not_speech(0.6, -1.5), "the bounds are strict");
        assert!(!not_speech(0.7, -1.0));
    }

    #[test]
    fn sound_tags_and_punctuation_are_not_words() {
        for t in ["*clap*", "*Clap*", "[music]", "[BLANK_AUDIO]", "(applause)", "(coughs).", "-", ".", "...", " - ", "\u{266a}\u{266a}", "<laughs>", ""] {
            assert!(sound_tag(t), "{t:?}");
        }
        for t in ["yes", "Stop.", "(not) now", "P-", "stop the build"] {
            assert!(!sound_tag(t), "{t:?}");
        }
    }

    #[test]
    fn the_phrases_whisper_hears_in_silence_are_known() {
        for t in ["you", "You", "Thank you.", "thank you", "Thanks for watching!", "Bye.", "www.mooji.org", "Um,"] {
            assert!(silence_phrase(t), "{t:?}");
        }
        for t in ["yes", "Stop.", "you should fix the parser", "thank you, merge it", "no", "okay", "come to www"] {
            assert!(!silence_phrase(t), "{t:?}");
        }
    }
}
