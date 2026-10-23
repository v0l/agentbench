//! The Whisper models foreman can read speech with.

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
