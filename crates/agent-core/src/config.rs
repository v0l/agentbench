use std::path::{Path, PathBuf};

pub const DEFAULT_URL: &str = "http://127.0.0.1:11434/v1";
pub const DEFAULT_STEPS: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub url: String,
    pub model: String,
    pub key: String,
    pub steps: usize,
    pub brief: String,
    pub workspace: String,
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
            rest: Vec::new(),
        }
    }
}

impl Config {
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
                other => c.rest.push((other.to_string(), value.to_string())),
            }
        }
        c
    }

    pub fn render(&self) -> String {
        format!(
            "url = {}\nmodel = {}\nkey = {}\nsteps = {}\nbrief = {}\nworkspace = {}\n{}",
            self.url,
            self.model,
            self.key,
            self.steps,
            self.brief.replace('\n', "\\n"),
            self.workspace,
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
