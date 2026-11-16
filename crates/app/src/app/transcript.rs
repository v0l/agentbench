//! The transcript: each message mapped onto the shared agent-ui entry widgets.

use super::*;
use agent_ui::transcript::{self as shared, Action, Check, File, Item};

struct Fetch<'a> {
    tray: &'a mut crate::attach::Tray,
    client: Option<&'a crate::hub::Hub>,
    session: SessionId,
}

fn attachment(f: &File) -> agentbench_core::Attachment {
    agentbench_core::Attachment {
        id: f.id,
        name: f.name.to_string(),
        mime: f.mime.to_string(),
        size: f.size,
    }
}

fn file(a: &agentbench_core::Attachment) -> File<'_> {
    File { id: a.id, name: &a.name, mime: &a.mime, size: a.size }
}

impl shared::Files for Fetch<'_> {
    fn thumb(&mut self, ctx: &egui::Context, f: &File) -> Option<shared::Thumb> {
        let c = self.client?;
        let session = self.session;
        let a = attachment(f);
        let thumb = self.tray.thumb(session, &a, |tx| {
            let (key, ctx) = ((session, a.id), ctx.clone());
            c.attachment(session, a.id, move |bytes| {
                let img = bytes.as_deref().and_then(crate::attach::decode);
                let _ = tx.send((key, img));
                ctx.request_repaint();
            });
        });
        Some(match thumb {
            shared::Thumb::Ready(t) => shared::Thumb::Ready(t.clone()),
            shared::Thumb::Loading => shared::Thumb::Loading,
            shared::Thumb::Failed => shared::Thumb::Failed,
        })
    }

    fn text(&mut self, ctx: &egui::Context, f: &File) -> Option<shared::Body> {
        let c = self.client?;
        let session = self.session;
        let a = attachment(f);
        let ctx = ctx.clone();
        let body = self.tray.body(session, &a, |tx| {
            let key = (session, a.id);
            c.attachment(session, a.id, move |bytes| {
                let text = bytes.map(|b| String::from_utf8_lossy(&b).into_owned());
                let _ = tx.send((key, text));
                ctx.request_repaint();
            });
        });
        Some(match body {
            shared::Body::Ready(t) => shared::Body::Ready(t.clone()),
            shared::Body::Loading => shared::Body::Loading,
            shared::Body::Failed => shared::Body::Failed,
        })
    }
}

fn question(q: &Question) -> shared::Question<'_> {
    shared::Question { text: &q.text, options: &q.options, multiple: q.multiple, secret: q.secret }
}

impl App {
    fn show_item(&mut self, ui: &mut Ui, session: SessionId, id: Id, item: &Item) -> Option<Action> {
        let mut fetch = Fetch { tray: &mut self.tray, client: self.client.as_ref(), session };
        let reading = self.reading.filter(|_| self.reader.busy());
        let mut show = shared::Show {
            markdown: &mut self.markdown,
            files: &mut fetch,
            now: agentbench_core::now_ms(),
            thinking: self.thinking,
            reading,
            read_aloud: !cfg!(target_arch = "wasm32"),
            save: !cfg!(target_arch = "wasm32"),
        };
        shared::entry(ui, &mut show, id, item)
    }

    pub(super) fn entry(&mut self, ui: &mut Ui, m: &Message, session: SessionId) {
        let diagnostics;
        let one_question;
        let many: Vec<shared::Question>;
        let files: Vec<File>;
        let item = match &m.entry {
            Entry::User { text, attachments, from } => {
                files = attachments.iter().map(file).collect();
                Item::You { text, files: &files, from, at: Some(m.created_at), queued: false }
            }
            Entry::Assistant { text } => Item::Said { text },
            Entry::Thinking { text } => Item::Thinking { text },
            Entry::Tool { name, input, output, error, .. } => {
                Item::Tool { name, input, output: output.as_deref(), error: *error, summary: None }
            }
            Entry::Question { id, question: q, answer } => {
                one_question = [question(q)];
                many = Vec::new();
                let _ = &many;
                let done = answer.as_ref().map(std::slice::from_ref);
                Item::Questions { id, questions: &one_question, answers: done }
            }
            Entry::Questions { id, questions, answers } => {
                many = questions.iter().map(question).collect();
                Item::Questions { id, questions: &many, answers: answers.as_deref() }
            }
            Entry::Error { text } => Item::Error { text },
            Entry::Notice { text } => Item::Notice { text },
            Entry::Warning { text } => Item::Warning { text },
            Entry::Diagnostics { text, .. } => {
                diagnostics = (
                    serde_json::json!({ "message": "language server" }),
                    agentbench_core::diagnostics_findings(text),
                );
                Item::Tool {
                    name: "diagnostics",
                    input: &diagnostics.0,
                    output: Some(&diagnostics.1),
                    error: false,
                    summary: None,
                }
            }
            Entry::File { text, file: f } => Item::File { caption: text, file: file(f) },
            Entry::Wire { .. } | Entry::Unknown => return,
            Entry::Summary { text } if text.is_empty() => {
                Item::Summary { title: "cleared: the agent starts afresh here", text: "" }
            }
            Entry::Section { text, covers, topic } => {
                diagnostics = (
                    serde_json::Value::Null,
                    format!("folded {covers} older messages (topic: {topic}) into a summary"),
                );
                Item::Summary { title: &diagnostics.1, text }
            }
            Entry::Summary { text } => {
                Item::Summary { title: "compacted: the agent carries on from this summary", text }
            }
        };
        let Some(action) = self.show_item(ui, session, m.id, &item) else { return };
        match (action, &m.entry) {
            (Action::Answer(a), Entry::Question { id, .. } | Entry::Questions { id, .. }) => {
                if let Some(c) = &mut self.client {
                    c.answer(session, id, &a);
                }
            }
            (Action::Read, Entry::Assistant { text }) => self.toggle_reading(ui.ctx(), m.id, text),
            (Action::Open, Entry::File { file, .. }) => self.open_viewer(ui.ctx(), session, file),
            (Action::Save, Entry::File { file, .. }) => self.save_file(ui.ctx(), session, file),
            _ => {}
        }
    }

    fn save_file(&mut self, ctx: &egui::Context, session: SessionId, file: &agentbench_core::Attachment) {
        let Some(c) = &self.client else { return };
        let (tx, ctx) = (self.tray.sender(), ctx.clone());
        let (name, mime) = (file.name.clone(), file.mime.clone());
        let key = (session, file.id);
        c.attachment(session, file.id, move |bytes| match bytes {
            Some(data) => crate::attach::save_keyed(tx, ctx, key, name, mime, data),
            None => {
                let _ = tx.send(crate::attach::Arrived::Failed(format!("could not download {name}")));
                ctx.request_repaint();
            }
        });
    }

    pub(super) fn queued(&mut self, ui: &mut Ui, session: SessionId, id: Id, text: &str) {
        let item = Item::You { text, files: &[], from: &[], at: None, queued: true };
        self.show_item(ui, session, id, &item);
    }
}

/// The newest checklist in a transcript: the agent's current plan.
pub(super) fn latest_plan(messages: &[Message]) -> Option<Vec<(Check, String)>> {
    messages.iter().rev().find_map(|m| match &m.entry {
        Entry::Tool { name, input, .. } => shared::checklist(name, input),
        _ => None,
    })
}
