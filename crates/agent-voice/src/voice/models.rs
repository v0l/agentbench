//! The Whisper models that can read speech with.

/// One model somebody can pick: id, Hugging Face repository, label, download size in MB.
pub type ModelInfo = (&'static str, &'static str, &'static str, u32);

/// The models Whisper can be run from here: id, Hugging Face repository, label, download size.
///
/// Smallest first. The tiny to small English ones read a microphone well on a CPU in a second or
/// two; medium and the large ones want a GPU (build with the `cuda` feature) to keep up. Sizes
/// are the safetensors each repository publishes. The multilingual models are told to expect
/// English.
pub const LOCAL_MODELS: &[ModelInfo] = &[
    ("whisper-tiny.en", "openai/whisper-tiny.en", "tiny, English (fastest)", 151),
    ("whisper-base.en", "openai/whisper-base.en", "base, English", 290),
    ("whisper-small.en", "openai/whisper-small.en", "small, English", 967),
    ("distil-whisper-small.en", "distil-whisper/distil-small.en", "distil small, English", 664),
    (
        "distil-whisper-medium.en",
        "distil-whisper/distil-medium.en",
        "distil medium, English (fast, accurate)",
        1580,
    ),
    ("whisper-medium.en", "openai/whisper-medium.en", "medium, English", 3060),
    ("whisper-tiny", "openai/whisper-tiny", "tiny, multilingual", 151),
    ("whisper-base", "openai/whisper-base", "base, multilingual", 290),
    ("whisper-small", "openai/whisper-small", "small, multilingual", 967),
    ("whisper-medium", "openai/whisper-medium", "medium, multilingual", 3060),
    (
        "distil-whisper-large-v3",
        "distil-whisper/distil-large-v3",
        "distil large v3, multilingual (fast, accurate)",
        3020,
    ),
    (
        "whisper-large-v3-turbo",
        "openai/whisper-large-v3-turbo",
        "large v3 turbo, multilingual (fast, accurate)",
        3230,
    ),
    ("whisper-large-v3", "openai/whisper-large-v3", "large v3, multilingual (most accurate)", 6170),
];

/// A voice the server can speak in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Voice {
    /// The published name, which is also the file fetched.
    pub id: &'static str,
    pub label: &'static str,
    /// The publisher's overall grade for the voice, A best.
    pub grade: &'static str,
}

/// Every voice offered, best graded first within each accent.
pub const VOICES: &[Voice] = &[
    Voice { id: "af_heart", label: "Heart, American", grade: "A" },
    Voice { id: "af_bella", label: "Bella, American", grade: "A-" },
    Voice { id: "af_nicole", label: "Nicole, American", grade: "B-" },
    Voice { id: "af_aoede", label: "Aoede, American", grade: "C+" },
    Voice { id: "af_kore", label: "Kore, American", grade: "C+" },
    Voice { id: "af_sarah", label: "Sarah, American", grade: "C+" },
    Voice { id: "am_fenrir", label: "Fenrir, American man", grade: "C+" },
    Voice { id: "am_michael", label: "Michael, American man", grade: "C+" },
    Voice { id: "am_puck", label: "Puck, American man", grade: "C+" },
    Voice { id: "am_echo", label: "Echo, American man", grade: "D" },
    Voice { id: "bf_emma", label: "Emma, British", grade: "B-" },
    Voice { id: "bf_isabella", label: "Isabella, British", grade: "C" },
    Voice { id: "bm_fable", label: "Fable, British man", grade: "C" },
    Voice { id: "bm_george", label: "George, British man", grade: "C" },
    Voice { id: "bm_lewis", label: "Lewis, British man", grade: "D+" },
];

/// The voice used when nobody picks one: the best graded of them.
pub const DEFAULT_VOICE: &str = "af_heart";

/// The voice a reply is read aloud in when none is chosen: one of Phonon's, which an app with
/// the `speech` feature speaks in (`speech::phonon`).
pub const DEFAULT_SPOKEN_VOICE: &str = "Hadley";

/// The model used when none is chosen.
pub const DEFAULT_MODEL: &str = "whisper-base.en";

pub fn model(id: &str) -> Option<&'static ModelInfo> {
    LOCAL_MODELS.iter().find(|m| m.0 == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_has_a_unique_id_and_repository() {
        for (i, a) in LOCAL_MODELS.iter().enumerate() {
            for b in &LOCAL_MODELS[i + 1..] {
                assert_ne!(a.0, b.0);
                assert_ne!(a.1, b.1);
            }
        }
    }

    #[test]
    fn the_default_model_is_on_the_list() {
        assert!(model(DEFAULT_MODEL).is_some());
        assert!(model("nonsense").is_none());
    }
}
