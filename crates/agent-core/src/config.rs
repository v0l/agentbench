use std::path::{Path, PathBuf};

pub const DEFAULT_URL: &str = "http://127.0.0.1:11434/v1";
pub const DEFAULT_STEPS: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
}

impl Sampling {
    pub fn over(self, base: Sampling) -> Sampling {
        Sampling {
            temperature: self.temperature.or(base.temperature),
            top_p: self.top_p.or(base.top_p),
            top_k: self.top_k.or(base.top_k),
            max_tokens: self.max_tokens.or(base.max_tokens),
        }
    }

    pub fn add_to(&self, body: &mut serde_json::Value) {
        let Some(map) = body.as_object_mut() else { return };
        if let Some(v) = self.temperature {
            map.insert("temperature".into(), v.into());
        }
        if let Some(v) = self.top_p {
            map.insert("top_p".into(), v.into());
        }
        if let Some(v) = self.top_k {
            map.insert("top_k".into(), v.into());
        }
        if let Some(v) = self.max_tokens {
            map.insert("max_tokens".into(), v.into());
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub url: String,
    pub model: String,
    pub key: String,
    pub steps: usize,
    pub brief: String,
    pub workspace: String,
    pub sampling: Sampling,
    pub thinking: Option<bool>,
    pub rest: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.into(),
            model: String::new(),
            key: String::new(),
            steps: DEFAULT_STEPS,
            brief: String::new(),
            workspace: String::new(),
            sampling: Sampling::default(),
            thinking: None,
            rest: Vec::new(),
        }
    }
}

fn parse_thinking(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Some(true),
        "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

impl Config {
    pub fn chat_template_kwargs(&self) -> Option<serde_json::Value> {
        self.thinking.map(|on| serde_json::json!({ "enable_thinking": on }))
    }

    pub fn path(dir: Option<&Path>) -> Option<PathBuf> {
        Some(dir?.join("agent"))
    }

    pub fn load(dir: Option<&Path>) -> Self {
        let text = Self::path(dir).and_then(|p| std::fs::read_to_string(p).ok());
        text.map(|t| Self::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self, dir: Option<&Path>) -> std::io::Result<()> {
        let Some(path) = Self::path(dir) else {
            return Err(std::io::Error::other("no config directory"));
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.render())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "url" => c.url = value.to_string(),
                "model" => c.model = value.to_string(),
                "key" => c.key = value.to_string(),
                "steps" => c.steps = value.parse().unwrap_or(DEFAULT_STEPS),
                "brief" => c.brief = value.replace("\\n", "\n"),
                "workspace" => c.workspace = value.to_string(),
                "temperature" => c.sampling.temperature = value.parse().ok(),
                "top_p" => c.sampling.top_p = value.parse().ok(),
                "top_k" => c.sampling.top_k = value.parse().ok(),
                "max_tokens" => c.sampling.max_tokens = value.parse().ok(),
                "thinking" => c.thinking = parse_thinking(value),
                other => c.rest.push((other.to_string(), value.to_string())),
            }
        }
        c
    }

    pub fn render(&self) -> String {
        let s = &self.sampling;
        let sampling: String = [
            ("temperature", s.temperature.map(|v| v.to_string())),
            ("top_p", s.top_p.map(|v| v.to_string())),
            ("top_k", s.top_k.map(|v| v.to_string())),
            ("max_tokens", s.max_tokens.map(|v| v.to_string())),
        ]
        .into_iter()
        .filter_map(|(k, v)| Some(format!("{k} = {}\n", v?)))
        .collect();
        format!(
            "url = {}\nmodel = {}\nkey = {}\nsteps = {}\nbrief = {}\nworkspace = {}\n{sampling}{}{}",
            self.url,
            self.model,
            self.key,
            self.steps,
            self.brief.replace('\n', "\\n"),
            self.workspace,
            match self.thinking {
                Some(on) => format!("thinking = {}\n", if on { "on" } else { "off" }),
                None => String::new(),
            },
            self.rest.iter().map(|(k, v)| format!("{k} = {v}\n")).collect::<String>()
        )
    }

    pub fn workspace_path(&self, app: &str) -> std::path::PathBuf {
        match self.workspace.trim() {
            "" => dirs::document_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join(app),
            given => std::path::PathBuf::from(given),
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.url.trim_end_matches('/'))
    }

    pub fn fault(&self) -> Option<&'static str> {
        if self.url.trim().is_empty() {
            return Some("no server address");
        }
        if self.model.trim().is_empty() {
            return Some("no model chosen");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_survives_a_round_trip() {
        let c = Config {
            url: "https://api.example.com/v1".into(),
            model: "some-model".into(),
            key: "sk-secret".into(),
            steps: 7,
            brief: "two\nlines".into(),
            workspace: "/work/parts".into(),
            sampling: Sampling {
                temperature: Some(0.6),
                top_p: Some(0.95),
                top_k: Some(20),
                max_tokens: Some(4096),
            },
            thinking: Some(false),
            rest: vec![("wake".into(), "shark".into())],
        };
        assert_eq!(Config::parse(&c.render()), c);
    }

    #[test]
    fn keys_the_agent_does_not_know_survive_a_save() {
        let text = "url = http://h/v1\nmodel = m\nwake = shark\nspeech = local\nvoice = af_sky\n";
        let c = Config::parse(text);
        assert_eq!(c.rest.len(), 3);
        let again = Config::parse(&c.render());
        assert_eq!(again.rest, c.rest);
        assert_eq!(again.model, "m");
        assert!(c.render().contains("wake = shark"));
    }

    #[test]
    fn sampling_set_in_config_beats_the_backend_default() {
        let set = Sampling { temperature: Some(0.2), ..Sampling::default() };
        let backend = Sampling { temperature: Some(1.0), top_k: Some(40), ..Sampling::default() };
        let got = set.over(backend);
        assert_eq!((got.temperature, got.top_k, got.top_p), (Some(0.2), Some(40), None));
        let mut body = serde_json::json!({ "model": "m" });
        got.add_to(&mut body);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_k"], 40);
        assert!(body.get("top_p").is_none() && body.get("max_tokens").is_none());
    }

    #[test]
    fn thinking_is_left_to_the_server_until_set() {
        assert_eq!(Config::default().chat_template_kwargs(), None);
        assert!(!Config::default().render().contains("thinking"));
        let on = Config::parse("thinking = on\n");
        assert_eq!(on.thinking, Some(true));
        assert_eq!(on.chat_template_kwargs().unwrap()["enable_thinking"], true);
        let off = Config::parse(&Config { thinking: Some(false), ..Config::default() }.render());
        assert_eq!(off.chat_template_kwargs().unwrap()["enable_thinking"], false);
        assert!(off.rest.is_empty());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Config::parse(""), Config::default());
        assert_eq!(Config::parse("nonsense\n[block]\n"), Config::default());
    }

    #[test]
    fn the_endpoint_takes_one_slash_however_the_url_ends() {
        let mut c = Config { url: "http://h/v1".into(), ..Config::default() };
        assert_eq!(c.endpoint(), "http://h/v1/chat/completions");
        c.url = "http://h/v1/".into();
        assert_eq!(c.endpoint(), "http://h/v1/chat/completions");
    }

    #[test]
    fn a_config_with_no_model_says_so() {
        assert_eq!(Config::default().fault(), Some("no model chosen"));
        assert_eq!(Config { model: "m".into(), ..Config::default() }.fault(), None);
    }
}
