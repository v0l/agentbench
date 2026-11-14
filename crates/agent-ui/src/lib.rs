#![cfg(not(target_arch = "wasm32"))]

pub mod chat;
pub mod pane;
pub mod settings;

pub use agent_core::{
    Ask, Backend, CancellationToken, Cx, Desk, Reply, Services, Welcome, async_trait, code,
    config, grants, http, jobs, local, lsp, runtime, sandbox, skills, skillset, spawn, todo,
    toolkit, tools, workspace,
};
#[cfg(feature = "web")]
pub use agent_core::web;
#[cfg(feature = "mcp")]
pub use agent_core::mcp;

use serde_json::Value;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::sync::oneshot;

pub type Poll<H> = Box<dyn FnMut(&mut H) -> Option<Reply>>;

pub fn desk(ctx: &egui::Context) -> (Desk, Receiver<Ask>) {
    let ctx = ctx.clone();
    Desk::new(move || ctx.request_repaint())
}

pub enum Handled<H> {
    Now(Reply),
    Later(Poll<H>),
}

pub trait Host: Sized {
    fn agent(&mut self) -> &mut Agent<Self>;

    fn call(&mut self, ctx: &egui::Context, name: &str, args: &Value) -> Handled<Self>;
}

pub struct Pending<H> {
    reply: oneshot::Sender<Reply>,
    poll: Poll<H>,
}

pub struct Agent<H> {
    pub chat: chat::Chat,
    pub desk: Desk,
    pub asks: Receiver<Ask>,
    pub open: bool,
    pub settings: Option<config::Config>,
    pub markdown: egui_bench::markdown::Cache,
    pub pending: Vec<Pending<H>>,
}

impl<H> Agent<H> {
    pub fn new(ctx: &egui::Context, backend: Arc<dyn Backend>, open: bool) -> Self {
        let (desk, asks) = desk(ctx);
        Self {
            chat: chat::Chat::new(backend),
            desk,
            asks,
            open,
            settings: None,
            markdown: egui_bench::markdown::Cache::default(),
            pending: Vec::new(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        pane::show(ui, self);
    }

    pub fn show_with(&mut self, ui: &mut egui::Ui, extras: &mut pane::Extras) {
        pane::show_with(ui, self, extras);
    }

    pub fn edit_settings(&mut self) {
        self.settings = Some(self.chat.config.clone());
    }

    pub fn set_config(&mut self, config: config::Config) {
        self.chat.config = config;
    }

    pub fn poll(&mut self) {
        self.chat.poll();
    }

    pub fn live(&self) -> bool {
        self.chat.busy() || !self.pending.is_empty()
    }

    pub fn needs_you(&self) -> bool {
        !self.chat.grants.pending().is_empty()
    }
}

pub fn serve<H: Host>(host: &mut H, ctx: &egui::Context) {
    let asks: Vec<Ask> = host.agent().asks.try_iter().collect();
    let any = !asks.is_empty();
    for ask in asks {
        match host.call(ctx, &ask.name, &ask.args) {
            Handled::Now(r) => {
                let _ = ask.reply.send(r);
            }
            Handled::Later(poll) => host.agent().pending.push(Pending { reply: ask.reply, poll }),
        }
    }
    let pending = std::mem::take(&mut host.agent().pending);
    for mut p in pending {
        match (p.poll)(host) {
            Some(r) => {
                let _ = p.reply.send(r);
            }
            None => host.agent().pending.push(p),
        }
    }
    if any || !host.agent().pending.is_empty() {
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}
