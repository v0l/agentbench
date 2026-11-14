//! How a reply is cut into what the model says in one go, and how long each may run. From
//! `ptts::plan` and `ptts::tts_model::split_into_best_sentences`.

use tokenizers::Tokenizer;

/// Tokens in one chunk, at most, unless a single sentence is longer.
pub const MAX_TOKENS_PER_CHUNK: usize = 50;

/// One piece of text the model says from a fresh start after the voice.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub text: String,
    pub tokens: Vec<u32>,
    /// Frames kept after the model says it is done: a short text gets a little more tail.
    pub frames_after_eos: usize,
    /// Frames allowed, at most, so a model that never says it is done still stops.
    pub frame_budget: usize,
}

pub fn frame_budget(tokens: usize, frame_rate: f64) -> usize {
    ((tokens as f64 / 3.0 + 2.0) * frame_rate).ceil() as usize
}

/// A capital first letter, one line, and a full stop if the text ends on a letter or digit.
pub fn prepare(text: &str) -> (String, usize) {
    let text = text.trim();
    if text.is_empty() {
        return (String::new(), 3);
    }
    let mut text: String =
        text.replace(['\n', '\r'], " ").split_whitespace().collect::<Vec<_>>().join(" ");
    let words = text.split_whitespace().count();
    let tail = if words <= 4 { 3 } else { 1 };
    let mut chars = text.chars();
    if let Some(first) = chars.next() {
        text = first.to_uppercase().collect::<String>() + chars.as_str();
    }
    if text.chars().last().is_some_and(|c| c.is_alphanumeric()) {
        text.push('.');
    }
    (text, tail)
}

fn encode(tok: &Tokenizer, text: &str) -> Result<Vec<u32>, String> {
    Ok(tok.encode(text, false).map_err(|e| e.to_string())?.get_ids().to_vec())
}

/// Sentences grouped into chunks of at most `max_tokens`, cut after `.`, `!`, `...` or `?`.
pub fn split(tok: &Tokenizer, text: &str, max_tokens: usize) -> Result<Vec<String>, String> {
    let (prepared, _) = prepare(text);
    let tokens = encode(tok, prepared.trim())?;
    let marks = encode(tok, ".!...?")?;
    let marks = if marks.len() > 1 { &marks[1..] } else { &marks[..] };
    let mut cuts = vec![0usize];
    let mut after_mark = false;
    for (i, t) in tokens.iter().enumerate() {
        if marks.contains(t) {
            after_mark = true;
        } else {
            if after_mark {
                cuts.push(i);
            }
            after_mark = false;
        }
    }
    cuts.push(tokens.len());
    let mut chunks = Vec::new();
    let (mut current, mut count) = (String::new(), 0usize);
    for w in cuts.windows(2) {
        let n = w[1] - w[0];
        let sentence = tok.decode(&tokens[w[0]..w[1]], true).map_err(|e| e.to_string())?;
        if current.is_empty() {
            (current, count) = (sentence, n);
        } else if count + n > max_tokens {
            chunks.push(current.trim().to_string());
            (current, count) = (sentence, n);
        } else {
            current.push(' ');
            current.push_str(&sentence);
            count += n;
        }
    }
    if !current.is_empty() {
        chunks.push(current.trim().to_string());
    }
    Ok(chunks)
}

/// The chunks for `text`, already normalized, each tokenized and given its budgets.
pub fn chunks(
    tok: &Tokenizer,
    text: &str,
    max_tokens: usize,
    frame_rate: f64,
) -> Result<Vec<Chunk>, String> {
    split(tok, text, max_tokens)?
        .into_iter()
        .map(|text| {
            let (prepared, frames_after_eos) = prepare(&text);
            let tokens = encode(tok, &prepared)?;
            let frame_budget = frame_budget(tokens.len(), frame_rate);
            Ok(Chunk { text, tokens, frames_after_eos, frame_budget })
        })
        .collect()
}

/// When to stop: once the model says it is done, `frames_after_eos` more frames, then stop.
#[derive(Clone, Copy, Debug)]
pub struct Eos {
    after: usize,
    countdown: Option<usize>,
}

impl Eos {
    pub fn new(after: usize) -> Eos {
        Eos { after, countdown: None }
    }

    pub fn should_stop(&mut self, is_eos: bool) -> bool {
        if is_eos && self.countdown.is_none() {
            self.countdown = Some(self.after);
        }
        match self.countdown.as_mut() {
            None => false,
            Some(0) => true,
            Some(c) => {
                *c -= 1;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_given_a_capital_and_a_full_stop() {
        assert_eq!(prepare("not a thing"), ("Not a thing.".into(), 3));
        assert_eq!(prepare("one two three four five"), ("One two three four five.".into(), 1));
        assert_eq!(prepare("is it?"), ("Is it?".into(), 3));
        assert_eq!(
            prepare("one\r\ntwo   three\nfour five"),
            ("One two three four five.".into(), 1)
        );
        assert_eq!(prepare("  "), (String::new(), 3));
    }

    #[test]
    fn the_end_of_speech_waits_its_frames_then_stops() {
        let mut e = Eos::new(2);
        assert!(!e.should_stop(false));
        assert!(!e.should_stop(true));
        assert!(!e.should_stop(false));
        assert!(e.should_stop(false));
    }
}
