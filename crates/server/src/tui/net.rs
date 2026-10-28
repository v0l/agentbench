use agentbench_core::{
    Answer, ApiError, Id, Message, NewSession, Overview, SendMessage, Session, SessionId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

const PATIENCE: Duration = Duration::from_secs(20);
pub const WATCH_WAIT: u64 = 20;

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

#[derive(Clone)]
pub struct Net {
    server: Server,
    http: reqwest::Client,
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

async fn read<T: DeserializeOwned>(r: reqwest::Response) -> Result<T, String> {
    let status = r.status();
    if status.is_success() {
        return r.json().await.map_err(|e| reason(&e));
    }
    Err(match r.json::<ApiError>().await {
        Ok(e) => e.error,
        Err(_) => status.to_string(),
    })
}

async fn done(r: reqwest::Response) -> Result<(), String> {
    if r.status().is_success() {
        return Ok(());
    }
    read::<serde_json::Value>(r).await.map(|_| ())
}

impl Net {
    pub fn new(server: Server) -> Net {
        Net { server, http: crate::http::client().clone() }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.server.base()))
            .bearer_auth(&self.server.token)
            .timeout(PATIENCE)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let r = self.request(reqwest::Method::GET, path).send().await.map_err(|e| reason(&e))?;
        read(r).await
    }

    async fn post<B: Serialize>(
        &self,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response, String> {
        let mut req = self.request(reqwest::Method::POST, path);
        if let Some(b) = body {
            req = req.json(b);
        }
        req.send().await.map_err(|e| reason(&e))
    }

    pub async fn overview(&self) -> Result<Overview, String> {
        self.get("/api/overview").await
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, String> {
        self.get("/api/sessions").await
    }

    pub async fn session(&self, id: SessionId) -> Result<Session, String> {
        self.get(&format!("/api/sessions/{id}")).await
    }

    pub async fn messages(&self, id: SessionId, after: Id) -> Result<Vec<Message>, String> {
        self.get(&format!("/api/sessions/{id}/messages?after={after}")).await
    }

    pub async fn changes(&self, since: Id) -> Result<Id, String> {
        #[derive(serde::Deserialize)]
        struct Changes {
            cursor: Id,
        }
        let r = self
            .request(reqwest::Method::GET, &format!("/api/changes?since={since}&wait={WATCH_WAIT}"))
            .timeout(Duration::from_secs(WATCH_WAIT + 10))
            .send()
            .await
            .map_err(|e| reason(&e))?;
        Ok(read::<Changes>(r).await?.cursor)
    }

    pub async fn create(&self, new: &NewSession) -> Result<Session, String> {
        read(self.post("/api/sessions", Some(new)).await?).await
    }

    pub async fn send(&self, id: SessionId, text: &str) -> Result<(), String> {
        let body = SendMessage { text: text.to_string(), attachments: Vec::new() };
        done(self.post(&format!("/api/sessions/{id}/messages"), Some(&body)).await?).await
    }

    pub async fn answer(
        &self,
        id: SessionId,
        question: &str,
        answers: &[String],
    ) -> Result<(), String> {
        let body = Answer {
            question: question.to_string(),
            answer: answers.join(", "),
            answers: answers.to_vec(),
        };
        done(self.post(&format!("/api/sessions/{id}/answer"), Some(&body)).await?).await
    }

    pub async fn interrupt(&self, id: SessionId) -> Result<Vec<String>, String> {
        let r = self.post::<()>(&format!("/api/sessions/{id}/interrupt"), None).await?;
        Ok(read::<agentbench_core::Interrupted>(r).await?.queued)
    }

    pub async fn delete(&self, id: SessionId) -> Result<(), String> {
        let r = self
            .request(reqwest::Method::DELETE, &format!("/api/sessions/{id}"))
            .send()
            .await
            .map_err(|e| reason(&e))?;
        done(r).await
    }
}
