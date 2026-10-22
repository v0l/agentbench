use ehttp::Method;
use foreman_core::{
    Comment, Id, IssuePatch, Message, NewComment, NewIssue, NewProject, NewSession, NewWorktree,
    Overview, SendMessage,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

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
    Model(foreman_core::ModelSettings),
    Models(Result<Vec<String>, String>),
    Changed(Id),
    Messages(Id, Vec<Message>),
    Comments(Id, Vec<Comment>),
    Done(String),
    Failed(String),
    Offline(String),
}

pub struct Client {
    pub server: Server,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    wake: Arc<dyn Fn() + Send + Sync>,
    pub pending: usize,
}

fn describe(r: &ehttp::Response) -> String {
    #[derive(serde::Deserialize)]
    struct Err {
        error: String,
    }
    r.json::<Err>().map(|e| e.error).unwrap_or_else(|_| format!("{} {}", r.status, r.status_text))
}

impl Client {
    pub fn new(server: Server, wake: impl Fn() + Send + Sync + 'static) -> Client {
        let (tx, rx) = channel();
        Client { server, tx, rx, wake: Arc::new(wake), pending: 0 }
    }

    pub fn poll(&mut self) -> Vec<Reply> {
        let replies: Vec<Reply> = self.rx.try_iter().collect();
        self.pending = self.pending.saturating_sub(replies.len());
        replies
    }

    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> ehttp::Request {
        let url = format!("{}{path}", self.server.base());
        let auth = format!("Bearer {}", self.server.token.trim());
        let mut headers = vec![("Accept", "application/json"), ("Authorization", auth.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let mut req = ehttp::Request::new(method, url, ehttp::Headers::new(&headers));
        req.body = body.unwrap_or_default();
        req
    }

    fn send<T: DeserializeOwned + 'static>(
        &mut self,
        req: ehttp::Request,
        done: impl FnOnce(T) -> Reply + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        self.pending += 1;
        ehttp::fetch(req, move |res| {
            let reply = match res {
                Err(e) => Reply::Offline(e),
                Ok(r) if r.status == 401 => Reply::Offline("the server refused the token".into()),
                Ok(r) if !r.ok => Reply::Failed(describe(&r)),
                Ok(r) if r.bytes.is_empty() => match serde_json::from_slice::<T>(b"null") {
                    Ok(v) => done(v),
                    Err(_) => Reply::Done(String::new()),
                },
                Ok(r) => match r.json::<T>() {
                    Ok(v) => done(v),
                    Err(e) => Reply::Failed(format!("unexpected reply: {e}")),
                },
            };
            let _ = tx.send(reply);
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
        let mut req =
            self.request(Method::GET, &format!("/api/changes?since={since}&wait=20"), None);
        req.timeout = Some(std::time::Duration::from_secs(40));
        self.send(req, |c: Changes| Reply::Changed(c.cursor));
    }

    pub fn messages(&mut self, session: Id, after: Id) {
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
        self.act(Method::POST, &format!("/api/issues/{issue}/start"), Some(new), "agent started");
    }

    pub fn add_session(&mut self, new: &NewSession) {
        self.act(Method::POST, "/api/sessions", Some(new), "agent started");
    }

    pub fn send_message(&mut self, session: Id, text: &str) {
        let body = SendMessage { text: text.to_string() };
        self.act(Method::POST, &format!("/api/sessions/{session}/messages"), Some(&body), "");
    }

    pub fn answer(&mut self, session: Id, question: &str, answer: &str) {
        let body =
            foreman_core::Answer { question: question.to_string(), answer: answer.to_string() };
        self.act(Method::POST, &format!("/api/sessions/{session}/answer"), Some(&body), "");
    }

    pub fn model(&mut self) {
        let req = self.request(Method::GET, "/api/settings/model", None);
        self.send(req, Reply::Model);
    }

    pub fn set_model(&mut self, m: &foreman_core::ModelSettings) {
        let req = self.request(Method::PUT, "/api/settings/model", Self::json(m));
        self.send(req, Reply::Model);
    }

    pub fn models(&mut self, m: &foreman_core::ModelSettings) {
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        let req = self.request(Method::POST, "/api/settings/models", Self::json(m));
        self.pending += 1;
        ehttp::fetch(req, move |res| {
            let reply = match res {
                Err(e) => Reply::Models(Err(e)),
                Ok(r) if !r.ok => Reply::Models(Err(describe(&r))),
                Ok(r) => Reply::Models(r.json::<Vec<String>>().map_err(|e| e.to_string())),
            };
            let _ = tx.send(reply);
            wake();
        });
    }

    pub fn interrupt(&mut self, session: Id) {
        self.act::<()>(
            Method::POST,
            &format!("/api/sessions/{session}/interrupt"),
            None,
            "stopping",
        );
    }

    pub fn delete_session(&mut self, session: Id) {
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
