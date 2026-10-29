use agentbench_core::{
    Comment, Id, IssuePatch, Message, NewComment, NewIssue, NewProject, NewSession, NewWorktree,
    Overview, SendMessage, SessionId,
};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use web_time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub url: String,
    pub token: String,
}

impl Server {
    pub fn base(&self) -> String {
        let url = self.url.trim().trim_end_matches('/');
        if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("http://{url}")
        }
    }
}

pub enum Reply {
    Overview(Overview),
    Started(agentbench_core::Session),
    Model(agentbench_core::ModelSettings),
    Title(agentbench_core::TitleSettings),
    /// Where the server runs its speech models. `None` when the server is too old to say.
    Audio(Option<agentbench_core::AudioStatus>),
    /// The voices the server reads aloud in, as (id, label). `None` when it cannot say.
    Voices(Option<Vec<(String, String)>>),
    /// The models the server reads speech with, as (id, label). `None` when it cannot say.
    SpeechModels(Option<Vec<(String, String)>>),
    TitleModels(Result<Vec<String>, String>),
    Models(Result<Vec<String>, String>),
    AgentModels(String, Result<Vec<String>, String>),
    Changed(Id),
    Messages(SessionId, Vec<Message>),
    Interrupted(SessionId, Vec<String>),
    Jobs(SessionId, Vec<agentbench_core::Job>),
    Comments(Id, Vec<Comment>),
    Done(String),
    Failed(String),
    Offline(String),
}

pub struct Client {
    pub server: Server,
    http: reqwest::Client,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    wake: Arc<dyn Fn() + Send + Sync>,
    patience: Duration,
    pub pending: usize,
}

/// How long an ordinary request may take before the window calls the server offline.
const PATIENCE: Duration = Duration::from_secs(20);
/// The long poll asks the server to hold for this long, and gives up a little after.
const WATCH_WAIT: u64 = 20;

async fn describe(r: reqwest::Response) -> String {
    #[derive(serde::Deserialize)]
    struct Err {
        error: String,
    }
    let status = r.status();
    match r.json::<Err>().await {
        Ok(e) => e.error,
        Err(_) => status.to_string(),
    }
}

fn reason(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn http() -> reqwest::Client {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("an http client")
}

#[cfg(target_arch = "wasm32")]
fn http() -> reqwest::Client {
    reqwest::Client::new()
}

impl Client {
    pub fn new(server: Server, wake: impl Fn() + Send + Sync + 'static) -> Client {
        let (tx, rx) = channel();
        Client {
            server,
            http: http(),
            tx,
            rx,
            wake: Arc::new(wake),
            patience: PATIENCE,
            pending: 0,
        }
    }

    /// How long an ordinary request may take before the server is called offline.
    pub fn with_patience(mut self, patience: Duration) -> Client {
        self.patience = patience;
        self
    }

    pub fn poll(&mut self) -> Vec<Reply> {
        let replies: Vec<Reply> = self.rx.try_iter().collect();
        self.pending = self.pending.saturating_sub(replies.len());
        replies
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.server.base());
        let mut req = self
            .http
            .request(method, url)
            .header("Accept", "application/json")
            .bearer_auth(self.server.token.trim())
            .timeout(self.patience);
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        req
    }

    /// Send a request in the background; its reply arrives through [`Client::poll`].
    fn send<T: DeserializeOwned + 'static>(
        &mut self,
        req: reqwest::RequestBuilder,
        done: impl FnOnce(T) -> Reply + Send + 'static,
    ) {
        self.dispatch(req, move |res| match res {
            Err(r) => r,
            Ok(bytes) if bytes.is_empty() => match serde_json::from_slice::<T>(b"null") {
                Ok(v) => done(v),
                Err(_) => Reply::Done(String::new()),
            },
            Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                Ok(v) => done(v),
                Err(e) => Reply::Failed(format!("unexpected reply: {e}")),
            },
        });
    }

    fn dispatch(
        &mut self,
        req: reqwest::RequestBuilder,
        finish: impl FnOnce(Result<Vec<u8>, Reply>) -> Reply + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        self.pending += 1;
        crate::spawn(async move {
            let res = match req.send().await {
                Err(e) => Err(Reply::Offline(reason(&e))),
                Ok(r) if r.status() == 401 => {
                    Err(Reply::Offline("the server refused the token".into()))
                }
                Ok(r) if !r.status().is_success() => Err(Reply::Failed(describe(r).await)),
                Ok(r) => match r.bytes().await {
                    Ok(b) => Ok(b.to_vec()),
                    Err(e) => Err(Reply::Offline(reason(&e))),
                },
            };
            let _ = tx.send(finish(res));
            wake();
        });
    }

    fn json<B: Serialize>(body: &B) -> Option<Vec<u8>> {
        serde_json::to_vec(body).ok()
    }

    pub fn overview(&mut self) {
        let req = self.request(Method::GET, "/api/overview", None);
        self.send(req, Reply::Overview);
    }

    pub fn watch(&mut self, since: Id) {
        #[derive(serde::Deserialize)]
        struct Changes {
            cursor: Id,
        }
        let req = self
            .request(Method::GET, &format!("/api/changes?since={since}&wait={WATCH_WAIT}"), None)
            .timeout(Duration::from_secs(WATCH_WAIT + 10));
        self.send(req, |c: Changes| Reply::Changed(c.cursor));
    }

    pub fn messages(&mut self, session: SessionId, after: Id) {
        let req = self.request(
            Method::GET,
            &format!("/api/sessions/{session}/messages?after={after}"),
            None,
        );
        self.send(req, move |m| Reply::Messages(session, m));
    }

    pub fn comments(&mut self, issue: Id) {
        let req = self.request(Method::GET, &format!("/api/issues/{issue}/comments"), None);
        self.send(req, move |c| Reply::Comments(issue, c));
    }

    fn act<B: Serialize>(&mut self, method: Method, path: &str, body: Option<&B>, said: &str) {
        let said = said.to_string();
        let req = self.request(method, path, body.and_then(Self::json));
        self.send(req, move |_: serde_json::Value| Reply::Done(said));
    }

    pub fn add_project(&mut self, new: &NewProject) {
        self.act(Method::POST, "/api/projects", Some(new), "project added");
    }

    pub fn remove_project(&mut self, id: Id) {
        self.act::<()>(Method::DELETE, &format!("/api/projects/{id}"), None, "project removed");
    }

    pub fn sync_project(&mut self, id: Id) {
        self.act::<()>(Method::POST, &format!("/api/projects/{id}/sync"), None, "");
    }

    pub fn add_worktree(&mut self, project: Id, new: &NewWorktree) {
        self.act(
            Method::POST,
            &format!("/api/projects/{project}/worktrees"),
            Some(new),
            "worktree made",
        );
    }

    pub fn remove_worktree(&mut self, id: Id, force: bool) {
        self.act::<()>(
            Method::DELETE,
            &format!("/api/worktrees/{id}?force={force}"),
            None,
            "worktree removed",
        );
    }

    pub fn add_issue(&mut self, new: &NewIssue) {
        self.act(Method::POST, "/api/issues", Some(new), "issue filed");
    }

    pub fn update_issue(&mut self, id: Id, patch: &IssuePatch) {
        self.act(Method::PATCH, &format!("/api/issues/{id}"), Some(patch), "");
    }

    pub fn delete_issue(&mut self, id: Id) {
        self.act::<()>(Method::DELETE, &format!("/api/issues/{id}"), None, "issue deleted");
    }

    pub fn add_comment(&mut self, issue: Id, new: &NewComment) {
        self.act(Method::POST, &format!("/api/issues/{issue}/comments"), Some(new), "");
    }

    pub fn start_issue(&mut self, issue: Id, new: &NewSession) {
        let req =
            self.request(Method::POST, &format!("/api/issues/{issue}/start"), Self::json(new));
        self.send(req, Reply::Started);
    }

    pub fn add_session(&mut self, new: &NewSession) {
        let req = self.request(Method::POST, "/api/sessions", Self::json(new));
        self.send(req, Reply::Started);
    }

    pub fn send_message(&mut self, session: SessionId, text: &str) {
        self.send_with(session, text, Vec::new());
    }

    pub fn send_with(
        &mut self,
        session: SessionId,
        text: &str,
        attachments: Vec<agentbench_core::Upload>,
    ) {
        let body = SendMessage { text: text.to_string(), attachments };
        self.act(Method::POST, &format!("/api/sessions/{session}/messages"), Some(&body), "");
    }

    pub fn answer(&mut self, session: SessionId, question: &str, answers: &[String]) {
        let body = agentbench_core::Answer {
            question: question.to_string(),
            answer: answers.join(", "),
            answers: answers.to_vec(),
        };
        self.act(Method::POST, &format!("/api/sessions/{session}/answer"), Some(&body), "");
    }

    pub fn model(&mut self) {
        let req = self.request(Method::GET, "/api/settings/model", None);
        self.send(req, Reply::Model);
    }

    pub fn set_model(&mut self, m: &agentbench_core::ModelSettings) {
        let req = self.request(Method::PUT, "/api/settings/model", Self::json(m));
        self.send(req, Reply::Model);
    }

    /// Ask the server where its speech models run. A server too old to have the endpoint gets
    /// `Audio(None)` back rather than an error in the status bar.
    pub fn audio_status(&mut self) {
        let req = self.request(Method::GET, "/v1/audio/status", None);
        self.dispatch(req, |res| match res {
            Ok(bytes) => Reply::Audio(serde_json::from_slice(&bytes).ok()),
            Err(_) => Reply::Audio(None),
        });
    }

    /// Ask the server which models it reads speech with. Each label carries its size, and says
    /// when the server has it already.
    pub fn speech_models(&mut self) {
        let req = self.request(Method::GET, "/v1/audio/models", None);
        self.dispatch(req, |res| {
            let list = res
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| {
                    v["data"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|m| {
                                let id = m["id"].as_str()?.to_string();
                                let mut label = m["label"].as_str().unwrap_or(&id).to_string();
                                if let Some(mb) = m["size_mb"].as_u64() {
                                    label.push_str(&format!(", {mb} MB"));
                                }
                                if m["installed"].as_bool() == Some(true) {
                                    label.push_str(", on the server");
                                }
                                Some((id, label))
                            })
                            .collect()
                    })
                });
            Reply::SpeechModels(list)
        });
    }

    /// Ask the server which voices it reads aloud in.
    pub fn voices(&mut self) {
        let req = self.request(Method::GET, "/v1/audio/voices", None);
        self.dispatch(req, |res| {
            let list = res
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| {
                    v["voices"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|v| {
                                Some((
                                    v["id"].as_str()?.to_string(),
                                    v["label"].as_str()?.to_string(),
                                ))
                            })
                            .collect()
                    })
                });
            Reply::Voices(list)
        });
    }

    pub fn title(&mut self) {
        let req = self.request(Method::GET, "/api/settings/title", None);
        self.send(req, Reply::Title);
    }

    pub fn set_title(&mut self, t: &agentbench_core::TitleSettings) {
        let req = self.request(Method::PUT, "/api/settings/title", Self::json(t));
        self.send(req, Reply::Title);
    }

    pub fn title_models(&mut self, t: &agentbench_core::TitleSettings) {
        let req = self.request(Method::POST, "/api/settings/title/models", Self::json(t));
        self.fetch_models(req, Reply::TitleModels);
    }

    pub fn models(&mut self, m: &agentbench_core::ModelSettings) {
        let req = self.request(Method::POST, "/api/settings/models", Self::json(m));
        self.fetch_models(req, Reply::Models);
    }

    pub fn agent_models(&mut self, agent: &str) {
        let req = self.request(Method::GET, &format!("/api/agents/{agent}/models"), None);
        let name = agent.to_string();
        self.fetch_models(req, move |list| Reply::AgentModels(name.clone(), list));
    }

    fn fetch_models(
        &mut self,
        req: reqwest::RequestBuilder,
        reply: impl Fn(Result<Vec<String>, String>) -> Reply + Send + 'static,
    ) {
        let req = req.timeout(Duration::from_secs(40));
        self.dispatch(req, move |res| {
            let list = match res {
                Err(Reply::Failed(e) | Reply::Offline(e)) => Err(e),
                Err(_) => Err("no reply".to_string()),
                Ok(bytes) => {
                    serde_json::from_slice::<Vec<String>>(&bytes).map_err(|e| e.to_string())
                }
            };
            reply(list)
        });
    }

    /// Fetch an attachment's bytes and hand them to `done` off the UI thread.
    pub fn attachment(
        &self,
        session: SessionId,
        id: Id,
        done: impl FnOnce(Option<Vec<u8>>) + Send + 'static,
    ) {
        let req = self
            .request(Method::GET, &format!("/api/sessions/{session}/attachments/{id}"), None)
            .header("Accept", "*/*")
            .timeout(Duration::from_secs(60));
        let wake = self.wake.clone();
        crate::spawn(async move {
            let bytes = match req.send().await {
                Ok(r) if r.status().is_success() => r.bytes().await.ok().map(|b| b.to_vec()),
                _ => None,
            };
            done(bytes);
            wake();
        });
    }

    pub fn interrupt(&mut self, session: SessionId) {
        let req = self.request(Method::POST, &format!("/api/sessions/{session}/interrupt"), None);
        self.send(req, move |r: agentbench_core::Interrupted| {
            Reply::Interrupted(session, r.queued)
        });
    }

    pub fn jobs(&mut self, session: SessionId) {
        let req = self.request(Method::GET, &format!("/api/sessions/{session}/jobs"), None);
        self.send(req, move |j| Reply::Jobs(session, j));
    }

    pub fn kill_job(&mut self, session: SessionId, handle: &str) {
        self.act::<()>(
            Method::POST,
            &format!("/api/sessions/{session}/jobs/{handle}/kill"),
            None,
            "stopped",
        );
    }

    pub fn delete_session(&mut self, session: SessionId) {
        self.act::<()>(
            Method::DELETE,
            &format!("/api/sessions/{session}"),
            None,
            "session removed",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_url_gets_a_scheme_and_loses_a_trailing_slash() {
        let s = |u: &str| Server { url: u.into(), token: String::new() }.base();
        assert_eq!(s("127.0.0.1:7420"), "http://127.0.0.1:7420");
        assert_eq!(s("https://fm.example.com/"), "https://fm.example.com");
        assert_eq!(s(" http://h:1 "), "http://h:1");
    }
}
