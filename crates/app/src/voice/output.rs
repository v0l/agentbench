#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    Headphones,
    Speakers(String),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Go,
    Ask,
    Refuse(String),
}

pub fn verdict(sink: &Sink, answered: Option<bool>, echo_cancel: bool) -> Verdict {
    let speakers = match (sink, answered) {
        (Sink::Headphones, _) | (Sink::Unknown, Some(true)) => return Verdict::Go,
        (Sink::Speakers(name), _) => name.clone(),
        (Sink::Unknown, Some(false)) => "speakers".to_string(),
        (Sink::Unknown, None) => return Verdict::Ask,
    };
    if echo_cancel {
        return Verdict::Go;
    }
    Verdict::Refuse(refusal(&speakers))
}

pub fn refusal(speakers: &str) -> String {
    format!(
        "hands free is off: the sound plays through {speakers}, not headphones, so the reply would feed back into the microphone. Plug in headphones, or turn on echo cancelling in settings."
    )
}

const HEADPHONE_WORDS: [&str; 4] = ["headphone", "headset", "handset", "hands-free"];
const SPEAKER_WORDS: [&str; 6] = ["speaker", "internal", "hdmi", "tv", "hifi", "car"];

fn has(text: &str, words: &[&str]) -> bool {
    let text = text.to_lowercase();
    words.iter().any(|w| text.contains(w))
}

pub fn classify(props: &[(String, String)], port: Option<&str>, description: &str) -> Sink {
    let get = |k: &str| {
        props
            .iter()
            .find(|(key, _)| key.replace('_', "-") == k.replace('_', "-"))
            .map(|(_, v)| v.as_str())
    };
    if let Some(p) = port {
        if has(p, &HEADPHONE_WORDS) {
            return Sink::Headphones;
        }
        if has(p, &SPEAKER_WORDS) {
            return Sink::Speakers(description.to_string());
        }
    }
    if let Some(ff) = get("device.form-factor") {
        if has(ff, &HEADPHONE_WORDS) {
            return Sink::Headphones;
        }
        if has(ff, &SPEAKER_WORDS) {
            return Sink::Speakers(description.to_string());
        }
    }
    let profile = get("api.bluez5.profile").unwrap_or_default();
    if profile.contains("headset") || profile.contains("hfp") || profile.contains("hsp") {
        return Sink::Headphones;
    }
    let name = get("node.name").unwrap_or_default();
    if name.contains("hdmi") || has(get("device.profile.name").unwrap_or_default(), &["hdmi"]) {
        return Sink::Speakers(description.to_string());
    }
    Sink::Unknown
}

pub fn from_pactl(default: &str, sinks_json: &str) -> Sink {
    let Ok(serde_json::Value::Array(sinks)) = serde_json::from_str(sinks_json) else {
        return Sink::Unknown;
    };
    let default = default.trim();
    let Some(sink) = sinks.iter().find(|s| s["name"].as_str() == Some(default)) else {
        return Sink::Unknown;
    };
    let props: Vec<(String, String)> = sink["properties"]
        .as_object()
        .map(|o| {
            o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
        })
        .unwrap_or_default();
    let active = sink["active_port"].as_str().unwrap_or_default();
    let port = sink["ports"].as_array().and_then(|ports| {
        ports.iter().find(|p| p["name"].as_str() == Some(active)).map(|p| {
            format!(
                "{} {} {}",
                p["name"].as_str().unwrap_or_default(),
                p["type"].as_str().unwrap_or_default(),
                p["description"].as_str().unwrap_or_default()
            )
        })
    });
    let port = port.or_else(|| (!active.is_empty()).then(|| active.to_string()));
    let description = sink["description"].as_str().unwrap_or(default);
    classify(&props, port.as_deref(), description)
}

pub fn from_wpctl(inspect: &str) -> Sink {
    let props: Vec<(String, String)> = inspect
        .lines()
        .filter_map(|l| {
            let (k, v) = l.trim().trim_start_matches('*').trim().split_once(" = ")?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect();
    if props.is_empty() {
        return Sink::Unknown;
    }
    let description = props
        .iter()
        .find(|(k, _)| k == "node.description")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "the default output".into());
    classify(&props, None, &description)
}

#[cfg(target_os = "linux")]
pub fn default_sink() -> Sink {
    let run = |cmd: &str, args: &[&str]| {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };
    if let (Some(default), Some(sinks)) = (
        run("pactl", &["get-default-sink"]),
        run("pactl", &["--format=json", "list", "sinks"]),
    ) {
        let sink = from_pactl(&default, &sinks);
        if sink != Sink::Unknown {
            return sink;
        }
    }
    run("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"]).map_or(Sink::Unknown, |o| from_wpctl(&o))
}

#[cfg(not(target_os = "linux"))]
pub fn default_sink() -> Sink {
    Sink::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = r#"[
      {"name":"alsa_output.pci-0000_00_1f.3.analog-stereo","description":"Built-in Audio Analog Stereo",
       "properties":{"device.form_factor":"internal","device.bus":"pci"},
       "ports":[{"name":"analog-output-speaker","description":"Speakers","type":"Speaker"},
                {"name":"analog-output-headphones","description":"Headphones","type":"Headphones"}],
       "active_port":"analog-output-headphones"},
      {"name":"alsa_output.hdmi","description":"HDMI Monitor","properties":{},
       "ports":[{"name":"hdmi-output-0","description":"HDMI / DisplayPort","type":"HDMI"}],
       "active_port":"hdmi-output-0"},
      {"name":"bluez_output.AA_BB.1","description":"WH-1000XM4",
       "properties":{"device.form_factor":"headset","device.bus":"bluetooth"},"ports":[],"active_port":null}
    ]"#;

    #[test]
    fn the_active_port_of_the_default_sink_decides() {
        let laptop = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        assert_eq!(from_pactl(laptop, PACTL), Sink::Headphones);
        let speakers = PACTL.replace(
            "\"active_port\":\"analog-output-headphones\"",
            "\"active_port\":\"analog-output-speaker\"",
        );
        assert_eq!(
            from_pactl(laptop, &speakers),
            Sink::Speakers("Built-in Audio Analog Stereo".into())
        );
        assert_eq!(from_pactl("alsa_output.hdmi\n", PACTL), Sink::Speakers("HDMI Monitor".into()));
        assert_eq!(from_pactl("bluez_output.AA_BB.1", PACTL), Sink::Headphones);
        assert_eq!(from_pactl("gone", PACTL), Sink::Unknown);
        assert_eq!(from_pactl("x", "not json"), Sink::Unknown);
    }

    #[test]
    fn an_audio_interface_with_no_form_factor_is_unknown() {
        let scarlett = r#"id 75, type PipeWire:Interface:Node
    alsa.card_name = "Scarlett 2i2 USB"
    device.api = "alsa"
  * media.class = "Audio/Sink"
  * node.description = "Focusrite Scarlett 2i2 2nd Gen Pro"
  * node.name = "alsa_output.usb-Focusrite_Scarlett_2i2_USB-00.pro-output-0.99""#;
        assert_eq!(from_wpctl(scarlett), Sink::Unknown);
        let buds = "  * node.name = \"bluez_output.11.1\"\n    api.bluez5.profile = \"headset-head-unit\"\n";
        assert_eq!(from_wpctl(buds), Sink::Headphones);
        let tv = "  * node.description = \"TV\"\n  * node.name = \"alsa_output.pci.hdmi-stereo\"\n";
        assert_eq!(from_wpctl(tv), Sink::Speakers("TV".into()));
        assert_eq!(from_wpctl(""), Sink::Unknown);
    }

    #[test]
    fn speakers_refuse_unknown_asks_once_and_an_answer_or_echo_cancelling_decides() {
        let speakers = Sink::Speakers("Built-in Audio".into());
        let Verdict::Refuse(why) = verdict(&speakers, None, false) else { panic!() };
        assert!(why.contains("Built-in Audio") && why.contains("headphones"), "{why}");
        assert_eq!(verdict(&speakers, Some(true), false), verdict(&speakers, None, false));
        assert_eq!(verdict(&speakers, None, true), Verdict::Go);
        assert_eq!(verdict(&Sink::Headphones, Some(false), false), Verdict::Go);
        assert_eq!(verdict(&Sink::Unknown, None, false), Verdict::Ask);
        assert_eq!(verdict(&Sink::Unknown, Some(true), false), Verdict::Go);
        assert!(matches!(verdict(&Sink::Unknown, Some(false), false), Verdict::Refuse(_)));
        assert_eq!(verdict(&Sink::Unknown, Some(false), true), Verdict::Go);
    }
}
