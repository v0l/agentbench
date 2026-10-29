//! The transcript: one entry per message, its tool rows and questions, and the text that summarises a tool call.

use super::*;

impl App {
    /// A card of one or more questions. A lone single-choice question is answered by
    /// clicking its option; anything else is filled in and sent with one button.
    pub(super) fn questions(
        &mut self,
        ui: &mut Ui,
        session: SessionId,
        id: &str,
        qs: &[Question],
        done: Option<&[String]>,
    ) {
        let rail = if done.is_some() { ETCH } else { READOUT };
        let quick = qs.len() == 1 && !qs[0].multiple && !qs[0].options.is_empty();
        let legend = if qs.len() == 1 { "question" } else { "questions" };
        card(
            ui,
            Some(rail),
            |ui| {
                Line::new().legend(legend).show(ui);
            },
            |ui| {
                let mut slots = self.drafts.remove(id).unwrap_or_default();
                slots.resize(qs.len(), Slot::default());
                let mut sent: Option<Vec<String>> = None;
                let mut enter = false;
                for (n, q) in qs.iter().enumerate() {
                    if n > 0 {
                        ui.add_space(10.0);
                    }
                    if qs.len() > 1 {
                        Line::new()
                            .legend(&format!("{} of {}", n + 1, qs.len()))
                            .size(10.0)
                            .show(ui);
                    }
                    let salt = id.len() * 7919 + session.0.as_u128() as usize + n * 104_729;
                    markdown::show(ui, &mut self.markdown, &q.text, salt);
                    if let Some(a) = done {
                        let said = a.get(n).map(String::as_str).unwrap_or_default();
                        Line::new().legend("you said").gap(8.0).set(said).size(12.0).wrapped(ui);
                        continue;
                    }
                    let slot = &mut slots[n];
                    for (i, o) in q.options.iter().enumerate() {
                        let on = slot.picked.contains(&i);
                        if !option_row(ui, o, q.multiple, on, (id, n, i)).clicked() {
                            continue;
                        }
                        if quick {
                            sent = Some(vec![o.clone()]);
                        } else if q.multiple {
                            if on {
                                slot.picked.retain(|p| *p != i);
                            } else {
                                slot.picked.push(i);
                                slot.picked.sort_unstable();
                            }
                        } else {
                            slot.picked = if on { vec![] } else { vec![i] };
                            slot.typed.clear();
                        }
                    }
                    let hint =
                        if q.options.is_empty() { "type an answer" } else { "or type an answer" };
                    let r = if q.secret {
                        secret(ui, &mut slot.typed)
                    } else if quick {
                        let ready = !slot.typed.trim().is_empty();
                        let mut go = false;
                        let r = field_then(ui, &mut slot.typed, hint, 90.0, |ui| {
                            go = ui.add(Key::new("answer").solid(READOUT).enabled(ready)).clicked();
                        });
                        if go && ready {
                            sent = Some(vec![slot.typed.trim().to_string()]);
                        }
                        r
                    } else {
                        field(ui, &mut slot.typed, hint)
                    };
                    if r.changed() && !q.multiple && !slot.typed.trim().is_empty() {
                        slot.picked.clear();
                    }
                    enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                }
                if done.is_none() && !quick {
                    let replies: Vec<String> =
                        qs.iter().zip(&slots).map(|(q, s)| s.reply(q)).collect();
                    let ready = replies.iter().all(|r| !r.is_empty());
                    ui.add_space(8.0);
                    let label = if qs.len() > 1 { "send answers" } else { "answer" };
                    let key = ui.add(Key::new(label).solid(READOUT).enabled(ready));
                    key.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, ready, label)
                    });
                    let go = key.clicked();
                    if (go || enter) && ready {
                        sent = Some(replies);
                    }
                } else if enter && quick {
                    let t = slots[0].typed.trim();
                    if !t.is_empty() {
                        sent = Some(vec![t.to_string()]);
                    }
                }
                match (sent, &mut self.client) {
                    (Some(a), Some(c)) => c.answer(session, id, &a),
                    _ => {
                        self.drafts.insert(id.to_string(), slots);
                    }
                }
            },
        );
    }

    pub(super) fn entry(&mut self, ui: &mut Ui, m: &Message, session: SessionId) {
        match &m.entry {
            Entry::User { text, attachments } => {
                self.you(ui, m.id, m.created_at, text, attachments, session, false)
            }
            Entry::Assistant { text } => {
                let mut read = false;
                let reading = self.is_reading(m.id);
                egui::Frame::NONE
                    .inner_margin(Margin { left: 13, right: 8, top: 2, bottom: 2 })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        markdown::show(ui, &mut self.markdown, text, m.id as usize);
                        if !cfg!(target_arch = "wasm32") && text.trim().len() > 1 {
                            read = read_link(ui, reading).clicked();
                        }
                    });
                if read {
                    self.toggle_reading(ui.ctx(), m.id, text);
                }
            }
            Entry::Thinking { text } => {
                if !self.thinking {
                    return;
                }
                rail_block(ui, egui::Color32::TRANSPARENT, egui::Color32::TRANSPARENT, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text.trim()).italics().size(12.0).color(LEGEND),
                        )
                        .wrap(),
                    );
                });
            }
            Entry::Tool { name, input, output, error, .. } => {
                self.tool_row(ui, m.id, name, input, output.as_deref(), *error)
            }
            Entry::Question { id, question, answer } => {
                let done = answer.as_ref().map(|a| [a.clone()]);
                self.questions(
                    ui,
                    session,
                    id,
                    std::slice::from_ref(question),
                    done.as_ref().map(|d| &d[..]),
                )
            }
            Entry::Questions { id, questions, answers } => {
                self.questions(ui, session, id, questions, answers.as_deref())
            }
            Entry::Error { text } => {
                rail_block(ui, FAULT, FAULT.gamma_multiply(0.08), |ui| {
                    Line::new().legend("error").tint(FAULT).size(10.0).show(ui);
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).font(mono(11.5)).color(FAULT))
                            .wrap(),
                    );
                });
            }
            Entry::Notice { text } => {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    Line::new().note(text.clone()).size(11.0).wrapped(ui);
                });
            }
            Entry::Diagnostics { text, .. } => self.tool_row(
                ui,
                m.id,
                "diagnostics",
                &serde_json::json!({ "message": "language server" }),
                Some(&agentbench_core::diagnostics_findings(text)),
                false,
            ),
            Entry::Wire { .. } | Entry::Unknown => {}
            Entry::Summary { text } if text.is_empty() => {
                heading(ui, "cleared: the agent starts afresh here");
            }
            Entry::Summary { text } => {
                heading(ui, "compacted: the agent carries on from this summary");
                rail_block(ui, OK, PANEL, |ui| {
                    markdown::show(ui, &mut self.markdown, text, m.id as usize);
                });
                ui.add_space(4.0);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn you(
        &mut self,
        ui: &mut Ui,
        id: Id,
        created_at: i64,
        text: &str,
        attachments: &[agentbench_core::Attachment],
        session: SessionId,
        queued: bool,
    ) {
        ui.add_space(6.0);
        let (rail, fill) =
            if queued { (LEGEND, egui::Color32::TRANSPARENT) } else { (READOUT, PANEL) };
        rail_block(ui, rail, fill, |ui| {
            if queued {
                Line::new()
                    .legend("you")
                    .size(10.0)
                    .gap(8.0)
                    .note("queued: the agent has not read this yet")
                    .size(10.0)
                    .show(ui);
            } else {
                Line::new()
                    .legend("you")
                    .size(10.0)
                    .gap(8.0)
                    .note(ago(created_at))
                    .size(10.0)
                    .show(ui);
            }
            ui.add_space(2.0);
            if !text.is_empty() {
                markdown::show(ui, &mut self.markdown, text, id as usize);
            }
            if !attachments.is_empty() {
                ui.add_space(4.0);
                self.attached(ui, session, attachments);
            }
        });
        ui.add_space(4.0);
    }

    /// The files a message carried: pictures as thumbnails, anything else as a named chip.
    pub(super) fn attached(
        &mut self,
        ui: &mut Ui,
        session: SessionId,
        files: &[agentbench_core::Attachment],
    ) {
        let client = self.client.as_ref();
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            for a in files {
                let label = format!("{} {}", a.name, crate::attach::size(a.size));
                if a.is_image()
                    && let Some(c) = client
                {
                    let thumb = self.tray.thumb(session, a, |tx| {
                        let (key, ctx) = ((session, a.id), ctx.clone());
                        c.attachment(session, a.id, move |bytes| {
                            let img = bytes.as_deref().and_then(crate::attach::decode);
                            let _ = tx.send((key, img));
                            ctx.request_repaint();
                        });
                    });
                    match thumb {
                        crate::attach::Thumb::Ready(tex) => {
                            let s = tex.size_vec2();
                            let h = s.y.min(160.0);
                            ui.add(egui::Image::new((
                                tex.id(),
                                Vec2::new(s.x * h / s.y.max(1.0), h),
                            )))
                            .on_hover_text(&label);
                            continue;
                        }
                        crate::attach::Thumb::Loading => {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                            spinner(ui, r.center(), 8.0, LEGEND);
                        }
                        crate::attach::Thumb::Failed => {}
                    }
                }
                chip(ui, &label, LEGEND, false);
            }
        });
    }

    pub(super) fn tool_row(
        &mut self,
        ui: &mut Ui,
        id: Id,
        name: &str,
        input: &serde_json::Value,
        output: Option<&str>,
        error: bool,
    ) {
        let (tint, state) = match (output, error) {
            (None, _) => (TRACE, "running"),
            (Some(_), true) => (FAULT, "failed"),
            (Some(_), false) => (LEGEND, ""),
        };
        let open_id = egui::Id::new(("tool-open", id));
        let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(error);
        let body = tool_body(name, input);
        // A shell command is the riskiest thing an agent does, so it is always shown whole and
        // wrapped, open or not, rather than cut to one line.
        let command = shell_command(input);
        let rail = if error {
            FAULT
        } else if output.is_none() {
            TRACE
        } else {
            ETCH
        };
        rail_block(ui, rail, egui::Color32::TRANSPARENT, |ui| {
            let head = ui
                .horizontal(|ui| {
                    let mark = if output.is_none() {
                        "◌"
                    } else if open {
                        "▾"
                    } else {
                        "▸"
                    };
                    ui.label(egui::RichText::new(mark).font(mono(11.0)).color(tint));
                    let mut line = Line::new().legend(name).size(10.0).tint(tint);
                    if command.is_none() {
                        line =
                            line.gap(8.0).value(tool_summary(name, input)).size(11.0).tint(VALUE);
                    }
                    if !state.is_empty() {
                        line = line.gap(10.0).legend(state).size(9.5).tint(tint);
                    }
                    line.elided(ui);
                })
                .response
                .interact(Sense::click());
            if head.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if head.clicked() {
                open = !open;
            }
            if let Some(c) = command {
                ui.horizontal(|ui| {
                    // Under the tool's name, not its marker.
                    ui.add_space(16.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("$ {c}")).font(mono(11.0)).color(VALUE),
                        )
                        .wrap(),
                    );
                });
            }
            if !open {
                if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
                    let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                    let more = out.lines().count().saturating_sub(1);
                    let tail = if more > 0 { format!("   +{more} lines") } else { String::new() };
                    ui.horizontal(|ui| {
                        // Under the tool's name, not its marker.
                        ui.add_space(16.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{}{tail}", first.trim()))
                                    .font(mono(10.5))
                                    .color(LEGEND.gamma_multiply(0.8)),
                            )
                            .truncate(),
                        );
                    });
                }
                return;
            }
            if let Some(b) = &body {
                ui.add_space(2.0);
                code_block(ui, b, VALUE, ("tool-in", id));
            }
            if let Some(out) = output {
                ui.add_space(4.0);
                let text = if out.trim().is_empty() { "no output" } else { out };
                code_block(ui, text, if error { FAULT } else { LEGEND }, ("tool-out", id));
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    }
}

/// The shell command a tool call runs, if it runs one: bash, and the other agents' versions of it.
pub(super) fn shell_command(input: &serde_json::Value) -> Option<&str> {
    input.get("command").and_then(|v| v.as_str()).filter(|c| !c.trim().is_empty())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Check {
    Todo,
    Doing,
    Done,
}

/// The items of a checklist call: ours (`todo_write`, `items`) or Claude's (`TodoWrite`, `todos`).
pub(super) fn checklist(name: &str, input: &serde_json::Value) -> Option<Vec<(Check, String)>> {
    if !matches!(name, "todo_write" | "TodoWrite") {
        return None;
    }
    let items = input.get("items").or_else(|| input.get("todos"))?.as_array()?;
    let list: Vec<(Check, String)> = items
        .iter()
        .filter_map(|i| {
            let text = i.get("text").or_else(|| i.get("content"))?.as_str()?.trim();
            let status = match i.get("status").and_then(|s| s.as_str()) {
                Some("done" | "completed") => Check::Done,
                Some("in_progress" | "doing") => Check::Doing,
                _ => Check::Todo,
            };
            Some((status, text.to_string()))
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

pub(super) fn tool_body(name: &str, input: &serde_json::Value) -> Option<String> {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str());
    // A command is drawn whole by the row itself, not as an extra block when it is open.
    if shell_command(input).is_some() {
        return None;
    }
    match name {
        "todo_write" | "TodoWrite" => {
            let lines: Vec<String> = checklist(name, input)?
                .into_iter()
                .map(|(c, t)| {
                    let mark = match c {
                        Check::Done => "[x]",
                        Check::Doing => "[~]",
                        Check::Todo => "[ ]",
                    };
                    format!("{mark} {t}")
                })
                .collect();
            Some(lines.join("\n"))
        }
        "edit_file" | "edit" => {
            let old = pick("old").or(pick("oldText"))?;
            let new = pick("new").or(pick("newText")).unwrap_or_default();
            let mut out = String::new();
            for l in old.lines() {
                out.push_str(&format!("- {l}\n"));
            }
            for l in new.lines() {
                out.push_str(&format!("+ {l}\n"));
            }
            Some(out)
        }
        "write_file" | "write" => {
            pick("content").map(|c| c.lines().take(40).collect::<Vec<_>>().join("\n"))
        }
        _ if input.is_object() && input.as_object().is_some_and(|o| o.len() > 1) => {
            serde_json::to_string_pretty(input).ok()
        }
        _ => None,
    }
}

pub(super) fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    if let Some(items) = checklist(name, input) {
        let done = items.iter().filter(|(c, _)| *c == Check::Done).count();
        return format!("{done} of {} done", items.len());
    }
    let text = pick("command")
        .or_else(|| pick("path"))
        .or_else(|| pick("file_path"))
        .or_else(|| pick("pattern"))
        .or_else(|| pick("query"))
        .or_else(|| pick("url"))
        .or_else(|| pick("handle"))
        .or_else(|| pick("message"))
        .or_else(|| pick("topic"))
        .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
    let line = text.lines().next().unwrap_or_default().to_string();
    let _ = name;
    line.chars().take(160).collect()
}

/// The newest checklist in a transcript: the agent's current plan.
pub(super) fn latest_plan(messages: &[Message]) -> Option<Vec<(Check, String)>> {
    messages.iter().rev().find_map(|m| match &m.entry {
        Entry::Tool { name, input, .. } => checklist(name, input),
        _ => None,
    })
}

/// The plan, pinned above the transcript so what the agent is on shows without scrolling. It
/// starts folded where room is short (a phone, a small window, a tile), and a click overrides that.
pub(super) fn plan_panel(
    ui: &mut Ui,
    session: SessionId,
    plan: &[(Check, String)],
    open_by_default: bool,
) {
    let done = plan.iter().filter(|(c, _)| *c == Check::Done).count();
    let open_id = egui::Id::new(("plan-open", session));
    let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(open_by_default);
    let doing = plan.iter().find(|(c, _)| *c == Check::Doing).map(|(_, t)| t.as_str());
    rail_block(ui, ETCH, egui::Color32::TRANSPARENT, |ui| {
        let head = ui
            .horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if open { "▾" } else { "▸" })
                        .font(mono(11.0))
                        .color(LEGEND),
                );
                let mut line = Line::new()
                    .legend("plan")
                    .size(10.0)
                    .tint(LEGEND)
                    .gap(8.0)
                    .value(format!("{done} of {} done", plan.len()))
                    .size(11.0)
                    .tint(VALUE);
                // Folded, the step in hand still shows.
                if let (false, Some(t)) = (open, doing) {
                    line = line.gap(10.0).value(t).size(11.0).tint(READOUT);
                }
                line.elided(ui);
            })
            .response
            .interact(Sense::click());
        if head.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if head.clicked() {
            open = !open;
        }
        if open {
            egui::ScrollArea::vertical()
                .id_salt(("plan-scroll", session))
                .max_height(150.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (status, text) in plan {
                        let (mark, tint) = match status {
                            Check::Done => ("[x]", LEGEND),
                            Check::Doing => ("[~]", READOUT),
                            Check::Todo => ("[ ]", VALUE),
                        };
                        ui.horizontal_top(|ui| {
                            ui.add_space(16.0);
                            ui.label(egui::RichText::new(mark).font(mono(11.0)).color(tint));
                            let mut t = egui::RichText::new(text).font(mono(11.0)).color(tint);
                            if *status == Check::Done {
                                t = t.strikethrough();
                            }
                            ui.add(egui::Label::new(t).wrap());
                        });
                    }
                });
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    ui.add_space(6.0);
}
