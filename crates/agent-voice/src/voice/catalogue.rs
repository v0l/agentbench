//! The Phonon checkpoint and its voice catalogue on disc: what is fetched, from where, and which
//! voices there are. The model is about 100 MB and each voice 250 KB, fetched when first used.

use std::path::{Path, PathBuf};

/// The checkpoint, pinned so a new upload cannot change the voice under the user.
pub const MODEL_REPO: &str = "gradium/phonon-31m-en";
pub const MODEL_REVISION: &str = "8c64b55ec301728e2d1695bfea87a08a267d0c2d";
pub const VOICE_REPO: &str = "gradium/phonon-voices";
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
pub const CATALOG: &str = "voices.jsonl";
/// The voice when none is chosen: an even, standard American female voice.
pub const DEFAULT_VOICE: &str = crate::voice::DEFAULT_SPOKEN_VOICE;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Voice {
    pub name: String,
    #[serde(default, deserialize_with = "or_empty")]
    pub voice_id: String,
    pub filename: String,
    #[serde(default, deserialize_with = "or_empty")]
    pub language: String,
    #[serde(default, deserialize_with = "or_empty")]
    pub locale: String,
    #[serde(default, deserialize_with = "or_empty")]
    pub gender: String,
    #[serde(default, deserialize_with = "or_empty")]
    pub description: String,
}

fn or_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(<Option<String> as serde::Deserialize>::deserialize(d)?.unwrap_or_default())
}

impl Voice {
    /// A short label: name, accent and gender, as a picker shows it.
    pub fn label(&self) -> String {
        let accent = match self.locale.as_str() {
            "en-us" => "American",
            "en-gb" => "British",
            "en-au" => "Australian",
            "en-ca" => "Canadian",
            "en-in" => "Indian",
            "en-ie" => "Irish",
            "en-za" => "South African",
            _ => "",
        };
        let man = if self.gender == "male" { " man" } else { "" };
        if accent.is_empty() { self.name.clone() } else { format!("{}, {accent}{man}", self.name) }
    }
}

pub fn model_url(file: &str) -> String {
    format!("https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_REVISION}/{file}")
}

pub fn voice_url(file: &str) -> String {
    format!("https://huggingface.co/{VOICE_REPO}/resolve/main/{file}")
}

/// The English voices in a catalogue file, in its order.
pub fn read_catalog(path: &Path) -> Result<Vec<Voice>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_catalog(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_catalog(text: &str) -> Result<Vec<Voice>, String> {
    let mut out = Vec::new();
    if text.trim_start().starts_with(|c| c != '{') {
        return Err("not a voice catalogue".into());
    }
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Voice = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("voice catalogue line {}: {e}", i + 1);
                continue;
            }
        };
        let plain = |s: &str| !s.is_empty() && !s.contains(['/', '\\']) && !s.contains("..");
        let file_ok = v.filename.strip_prefix("voices/").is_some_and(plain);
        // Character voices (a pirate, a wizard) are listed as English too; a reply is not read
        // in one.
        let accent = v.locale.starts_with("en-");
        if v.language == "en" && accent && file_ok && plain(&v.name) {
            out.push(v);
        }
    }
    Ok(out)
}

/// Where a voice's file sits under `dir`.
pub fn voice_path(dir: &Path, v: &Voice) -> PathBuf {
    dir.join(&v.filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_keeps_english_voices_with_safe_names() {
        let text = r#"{"filename": "voices/a1.safetensors", "name": "Hadley", "voice_id": "a1", "language": "en", "locale": "en-us", "gender": "female"}
{"filename": "voices/b2.safetensors", "name": "Lucie", "voice_id": "b2", "language": "fr", "locale": "fr-fr"}
{"filename": "../../etc/passwd", "name": "Evil", "voice_id": "c3", "language": "en"}
{"filename": "voices/d4.safetensors", "name": "Alfie", "voice_id": "d4", "language": "en", "locale": "en-gb", "gender": "male"}
"#;
        let voices = parse_catalog(text).unwrap();
        let names: Vec<_> = voices.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Hadley", "Alfie"], "French and path-escaping entries are dropped");
        assert_eq!(voices[0].label(), "Hadley, American");
        assert_eq!(voices[1].label(), "Alfie, British man");
        assert!(parse_catalog("not json").is_err());
    }

    /// The published catalogue has a voice with no description (`null`), which failed the whole
    /// catalogue and left the server with no voices at all.
    #[test]
    fn a_voice_with_a_missing_field_does_not_lose_the_others() {
        let text = r#"{"filename": "voices/a1.safetensors", "name": "Hadley", "voice_id": "a1", "language": "en", "locale": "en-us", "gender": "female", "description": "even"}
{"filename": "voices/b2.safetensors", "name": "Odd", "voice_id": "b2", "language": "en", "locale": "en-us", "gender": null, "age": null, "description": null}
{"filename": "voices/c3.safetensors", "name": "Captain", "voice_id": "c3", "language": "en", "locale": "characters", "gender": null}
"#;
        let names: Vec<_> = parse_catalog(text).unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["Hadley", "Odd"]);
    }
}
