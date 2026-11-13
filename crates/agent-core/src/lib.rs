#![cfg(not(target_arch = "wasm32"))]

pub mod code;
pub mod config;
pub mod grants;
pub mod http;
pub mod jobs;
pub mod local;
pub mod lsp;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod sandbox;
pub mod skills;
pub mod skillset;
pub mod spawn;
pub mod todo;
pub mod toolkit;
pub mod tools;
pub mod web;
pub mod workspace;

pub use async_trait::async_trait;
pub use tokio_util::sync::CancellationToken;

use serde_json::Value;
use skillset::Skill;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;
use workspace::Workspace;

pub type Reply = Result<Value, String>;

const PATIENCE: Duration = Duration::from_secs(900);

pub fn runtime() -> tokio::runtime::Handle {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle;
    }
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("agent-core")
                .enable_all()
                .build()
                .expect("a tokio runtime for the agent")
        })
        .handle()
        .clone()
}

pub struct Ask {
    pub name: String,
    pub args: Value,
    pub reply: oneshot::Sender<Reply>,
}

#[derive(Clone)]
pub struct Desk {
    jobs: Sender<Ask>,
    ring: Arc<dyn Fn() + Send + Sync>,
}

impl Desk {
    pub fn new(ring: impl Fn() + Send + Sync + 'static) -> (Self, Receiver<Ask>) {
        let (jobs, asks) = channel();
        (Self { jobs, ring: Arc::new(ring) }, asks)
    }

    pub fn ring(&self) {
        (self.ring)();
    }

    pub async fn ask(&self, name: &str, args: Value, stop: &CancellationToken) -> Reply {
        let (reply, answer) = oneshot::channel();
        let ask = Ask { name: name.to_string(), args, reply };
        self.jobs.send(ask).map_err(|_| "the window has gone".to_string())?;
        self.ring();
        tokio::select! {
            r = answer => r.unwrap_or_else(|_| Err("the window dropped the request".into())),
            _ = stop.cancelled() => Err("stopped".into()),
            _ = tokio::time::sleep(PATIENCE) => Err(format!("the window did not answer in {PATIENCE:?}")),
        }
    }
}

#[derive(Default)]
pub struct Services {
    pub jobs: jobs::Jobs,
    pub code: Arc<code::Code>,
    pub reader: Option<Arc<dyn web::Reader>>,
}

impl Services {
    pub fn with_code(mut self, settings: code::Settings) -> Self {
        self.code = Arc::new(code::Code::new(settings));
        self
    }

    pub fn with_reader(mut self, reader: Arc<dyn web::Reader>) -> Self {
        self.reader = Some(reader);
        self
    }
}

#[derive(Clone)]
pub struct Cx {
    pub workspace: Workspace,
    pub desk: Desk,
    pub stop: CancellationToken,
    pub services: Arc<Services>,
}

impl Cx {
    pub fn new(workspace: Workspace, desk: Desk) -> Self {
        Self {
            workspace,
            desk,
            stop: CancellationToken::new(),
            services: Arc::new(Services::default()),
        }
    }

    pub fn with_services(mut self, services: Arc<Services>) -> Self {
        self.services = services;
        self
    }

    pub async fn ask(&self, name: &str, args: Value) -> Reply {
        self.desk.ask(name, args, &self.stop).await
    }

    pub fn ask_blocking(&self, name: &str, args: Value) -> Reply {
        runtime().block_on(self.ask(name, args))
    }
}

pub struct Welcome {
    pub kicker: String,
    pub title: String,
    pub text: String,
    pub placeholder: String,
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn app(&self) -> &'static str;

    fn config_dir(&self) -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join(self.app()))
    }

    fn user_agent(&self) -> String {
        format!("{}/{}", self.app(), env!("CARGO_PKG_VERSION"))
    }

    fn brief(&self, workspace: &Path, skills: &[Skill]) -> String;

    fn welcome(&self) -> Welcome;

    fn toolkit(&self) -> &toolkit::Toolkit {
        toolkit::empty()
    }

    fn specs(&self) -> Vec<tools::Spec> {
        self.toolkit().specs()
    }

    async fn call(&self, name: &str, args: &Value, cx: &Cx) -> Option<Result<String, String>> {
        self.toolkit().call(name, args, cx).await
    }

    fn preferred_skills(&self) -> Vec<String> {
        Vec::new()
    }

    async fn tool_dirs(&self, _cx: &Cx) -> Vec<PathBuf> {
        Vec::new()
    }

    fn leave_out(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn summary(&self, name: &str, args: &Value) -> Option<String> {
        self.toolkit().summary(name, args)
    }
}
