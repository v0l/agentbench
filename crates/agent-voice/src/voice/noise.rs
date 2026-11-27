pub const NO_SPEECH_THRESHOLD: f32 = 0.6;
pub const LOGPROB_THRESHOLD: f32 = -1.0;
pub const COMPRESSION_RATIO_THRESHOLD: f32 = 2.4;

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

pub fn repetition_loop(text: &str) -> bool {
    use std::io::Write;
    let bytes = text.trim().as_bytes();
    if bytes.len() < 24 {
        return false;
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    if z.write_all(bytes).is_err() {
        return false;
    }
    z.finish().is_ok_and(|c| bytes.len() as f32 / c.len().max(1) as f32 > COMPRESSION_RATIO_THRESHOLD)
}

pub fn silence_phrase(text: &str) -> bool {
    let plain: String = text
        .chars()
        .flat_map(|c| {
            let kept = if c.is_alphanumeric() || c == '\'' { c } else { ' ' };
            kept.to_lowercase()
        })
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
        || OTHER_LANGUAGES.contains(&plain.as_str())
        || SUBTITLE_CREDITS.iter().any(|c| plain.starts_with(c))
}

const OTHER_LANGUAGES: &[&str] = &[
    "ご視聴ありがとうございました",
    "ご視聴ありがとうございます",
    "ご清聴ありがとうございました",
    "ありがとうございました",
    "チャンネル登録をお願いします",
    "チャンネル登録お願いします",
    "最後までご視聴いただきありがとうございました",
    "시청해주셔서 감사합니다",
    "시청해 주셔서 감사합니다",
    "감사합니다",
    "구독과 좋아요 부탁드립니다",
    "谢谢观看",
    "感谢观看",
    "谢谢大家",
    "感謝觀看",
    "謝謝觀看",
    "请不吝点赞 订阅 转发 打赏支持明镜与点点栏目",
    "請不吝點贊 訂閱 轉發 打賞支持明鏡與點點欄目",
    "gracias por ver el video",
    "gracias por ver",
    "suscríbete",
    "suscríbete al canal",
    "danke fürs zuschauen",
    "vielen dank fürs zuschauen",
    "vielen dank für ihre aufmerksamkeit",
    "merci d'avoir regardé cette vidéo",
    "merci d'avoir regardé",
    "abonnez vous",
    "obrigado por assistir",
    "obrigada por assistir",
    "inscreva se no canal",
    "спасибо за просмотр",
    "спасибо за внимание",
    "подписывайтесь на канал",
    "продолжение следует",
    "grazie per la visione",
    "bedankt voor het kijken",
];

const SUBTITLE_CREDITS: &[&str] = &[
    "subtitles by",
    "subtitles made by",
    "subtitled by",
    "captions by",
    "amara org",
    "untertitel von",
    "untertitel der amara org",
    "untertitelung des zdf",
    "sous titres réalisés par",
    "sous titrage",
    "subtítulos realizados por",
    "subtítulos por",
    "legendas pela comunidade",
    "субтитры сделал",
    "субтитры создавал",
    "редактор субтитров",
    "字幕由",
    "자막 제공",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repetition_loop_is_caught_and_ordinary_sentences_are_not() {
        let looped = format!("Продолжение {}", "а ".repeat(220));
        let stutter = "а".repeat(444);
        assert!(repetition_loop(&looped));
        assert!(repetition_loop(&stutter));
        for t in ["yes", "Stop.", "please open the parser file and fix the failing test in the lexer module", "ご視聴ありがとうございました"] {
            assert!(!repetition_loop(t), "{t:?}");
        }
    }

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
        for t in [
            "ご視聴ありがとうございました",
            "ご視聴ありがとうございました。",
            "チャンネル登録をお願いします",
            "시청해주셔서 감사합니다.",
            "谢谢观看",
            "请不吝点赞 订阅 转发 打赏支持明镜与点点栏目",
            "Gracias por ver el video.",
            "Danke fürs Zuschauen!",
            "Untertitel der Amara.org-Community",
            "Merci d'avoir regardé cette vidéo !",
            "Sous-titres réalisés par la communauté d'Amara.org",
            "Obrigado por assistir!",
            "Legendas pela comunidade Amara.org",
            "Спасибо за просмотр!",
            "Субтитры сделал DimaTorzok",
            "Продолжение следует...",
            "Subtitles by the Amara.org community",
        ] {
            assert!(silence_phrase(t), "{t:?}");
        }
        for t in ["はい", "止めて", "停止", "detente", "hör auf", "arrête", "pare", "стоп", "ご視聴ありがとうございました、次のファイルを開いて"] {
            assert!(!silence_phrase(t), "{t:?}");
        }
        for t in ["yes", "Stop.", "you should fix the parser", "thank you, merge it", "no", "okay", "come to www"] {
            assert!(!silence_phrase(t), "{t:?}");
        }
    }
}
