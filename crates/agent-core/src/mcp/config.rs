use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Server {
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub from: PathBuf,
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub bearer_token: String,
    pub bearer_token_env: String,
    pub disabled: bool,
    pub timeout: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stdio,
    Http,
}

impl Server {
    pub fn kind(&self) -> Result<Kind, String> {
        match self.kind.trim().to_ascii_lowercase().as_str() {
            "sse" => Err(format!(
                "{} uses the old SSE transport, which is not supported; point it at the server's streamable HTTP address",
                self.name
            )),
            "stdio" | "local" if !self.command.trim().is_empty() => Ok(Kind::Stdio),
            "http" | "streamable-http" | "streamable_http" | "streamablehttp" | "remote"
                if !self.url.trim().is_empty() =>
            {
                Ok(Kind::Http)
            }
            "" if !self.command.trim().is_empty() => Ok(Kind::Stdio),
            "" if !self.url.trim().is_empty() => Ok(Kind::Http),
            _ => Err(format!("{} has no command to run and no url to reach", self.name)),
        }
    }

    pub fn shown(&self) -> String {
        match self.kind() {
            Ok(Kind::Http) => self.url.clone(),
            _ => {
                let mut words = vec![self.command.clone()];
                words.extend(self.args.iter().cloned());
                let line = words.join(" ");
                match line.char_indices().nth(80) {
                    Some((i, _)) => format!("{}...", &line[..i]),
                    None => line,
                }
            }
        }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> =
            self.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let token = match self.bearer_token.trim() {
            "" => std::env::var(self.bearer_token_env.trim()).unwrap_or_default(),
            t => t.to_string(),
        };
        if !token.trim().is_empty()
            && !out.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        {
            out.push(("Authorization".into(), format!("Bearer {}", token.trim())));
        }
        out
    }

    fn expanded(mut self, problems: &mut Vec<String>) -> Server {
        let name = self.name.clone();
        let mut ex = |s: &str| expand(s, &name, problems);
        self.command = home(&ex(&self.command));
        self.args = self.args.iter().map(|a| home(&ex(a))).collect();
        self.cwd = home(&ex(&self.cwd));
        self.url = ex(&self.url);
        self.bearer_token = ex(&self.bearer_token);
        self.env = self.env.iter().map(|(k, v)| (k.clone(), ex(v))).collect();
        self.headers = self.headers.iter().map(|(k, v)| (k.clone(), ex(v))).collect();
        self
    }
}

fn home(s: &str) -> String {
    match (s.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(h)) => h.join(rest).display().to_string(),
        _ if s == "~" => {
            dirs::home_dir().map_or_else(|| s.to_string(), |h| h.display().to_string())
        }
        _ => s.to_string(),
    }
}

pub fn expand(text: &str, server: &str, problems: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[at..]);
            return out;
        };
        let inner = &after[..end];
        let (var, default) = match inner.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (inner, None),
        };
        match (std::env::var(var.trim()), default) {
            (Ok(v), _) if !v.is_empty() => out.push_str(&v),
            (_, Some(d)) => out.push_str(d),
            _ => problems.push(format!("{server}: ${{{var}}} is not set")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

pub fn read(path: &Path) -> Result<Vec<Server>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let table = v
        .get("mcpServers")
        .or_else(|| v.get("servers"))
        .or_else(|| v.pointer("/mcp/servers"))
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{}: no \"mcpServers\" object", path.display()))?;
    let mut out = Vec::new();
    for (name, entry) in table {
        let mut server: Server = serde_json::from_value(entry.clone())
            .map_err(|e| format!("{}: server {name}: {e}", path.display()))?;
        server.name = name.clone();
        server.from = path.to_path_buf();
        out.push(server);
    }
    Ok(out)
}

pub fn load(files: &[PathBuf]) -> (Vec<Server>, Vec<String>) {
    let mut servers: Vec<Server> = Vec::new();
    let mut problems = Vec::new();
    for file in files {
        match read(file) {
            Ok(found) => {
                for s in found {
                    servers.retain(|o| o.name != s.name);
                    servers.push(s);
                }
            }
            Err(e) => problems.push(e),
        }
    }
    servers.retain(|s| !s.disabled);
    let servers = servers.into_iter().map(|s| s.expanded(&mut problems)).collect();
    (servers, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_format_is_read_and_a_later_file_replaces_a_server_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("mcp.json");
        let project = dir.path().join(".mcp.json");
        std::fs::write(
            &global,
            r#"{ "mcpServers": {
                "docs": { "url": "https://example.com/mcp", "headers": { "X-Key": "k" } },
                "kicad": { "command": "node", "args": ["index.js"], "env": { "A": "1" }, "lifecycle": "lazy" },
                "old": { "type": "sse", "url": "https://example.com/sse" },
                "off": { "command": "x", "disabled": true }
            } }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{ "servers": { "kicad": { "type": "stdio", "command": "kicad-mcp" } } }"#,
        )
        .unwrap();
        let (servers, problems) =
            load(&[global.clone(), project.clone(), dir.path().join("none.json")]);
        assert!(problems.is_empty(), "{problems:?}");
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["docs", "old", "kicad"]);
        assert_eq!(servers[0].kind(), Ok(Kind::Http));
        assert_eq!(servers[0].headers(), [("X-Key".to_string(), "k".to_string())]);
        assert!(servers[1].kind().unwrap_err().contains("old SSE transport"));
        assert_eq!(servers[2].kind(), Ok(Kind::Stdio));
        assert_eq!(servers[2].command, "kicad-mcp");
        assert_eq!(servers[2].from, project);
    }

    #[test]
    fn variables_are_filled_in_and_a_missing_one_is_reported() {
        unsafe { std::env::set_var("EGUI_BENCH_MCP_TEST_TOKEN", "s3cret") };
        let mut problems = Vec::new();
        assert_eq!(
            expand("Bearer ${EGUI_BENCH_MCP_TEST_TOKEN}", "ha", &mut problems),
            "Bearer s3cret"
        );
        assert_eq!(
            expand("${EGUI_BENCH_MCP_NOPE:-http://h}/mcp", "ha", &mut problems),
            "http://h/mcp"
        );
        assert!(problems.is_empty());
        assert_eq!(expand("a${EGUI_BENCH_MCP_NOPE}b", "ha", &mut problems), "ab");
        assert_eq!(problems, ["ha: ${EGUI_BENCH_MCP_NOPE} is not set"]);
        assert_eq!(expand("no ${end", "ha", &mut problems), "no ${end");
    }

    #[test]
    fn a_bearer_token_becomes_an_authorization_header_unless_one_is_set() {
        unsafe { std::env::set_var("EGUI_BENCH_MCP_TEST_BEARER", "tok") };
        let s =
            Server { bearer_token_env: "EGUI_BENCH_MCP_TEST_BEARER".into(), ..Server::default() };
        assert_eq!(s.headers(), [("Authorization".to_string(), "Bearer tok".to_string())]);
        let mut s = Server { bearer_token: "a".into(), ..Server::default() };
        s.headers.insert("authorization".into(), "Basic x".into());
        assert_eq!(s.headers(), [("authorization".to_string(), "Basic x".to_string())]);
    }

    #[test]
    fn a_broken_file_is_a_problem_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("mcp.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let good = dir.path().join(".mcp.json");
        std::fs::write(&good, r#"{"mcpServers": {"a": {"command": "a"}}}"#).unwrap();
        let (servers, problems) = load(&[bad, good]);
        assert_eq!(servers.len(), 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("mcp.json"), "{problems:?}");
    }

    #[test]
    fn a_server_with_neither_command_nor_url_says_so() {
        let s = Server { name: "x".into(), ..Server::default() };
        assert!(s.kind().unwrap_err().contains("no command"));
    }
}
