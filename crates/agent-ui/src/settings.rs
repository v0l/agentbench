use super::config::Config;
use crate::key::Key;
use egui_bench::prelude::*;
use egui::Ui;
use std::path::Path;

pub enum Done {
    Save(Config),
    Cancel,
}

pub fn show(ui: &mut Ui, draft: &mut Config, app: &str, dir: Option<&Path>) -> Option<Done> {
    let mut done = None;
    ui.add_space(8.0);
    modal_title(ui, "model");
    hint(
        ui,
        "Any server speaking the OpenAI chat completions API with tool calls: OpenAI, \
         OpenRouter, llama.cpp, vLLM, or Ollama on this machine.",
    );
    ui.add_space(10.0);
    ui.label(legend("server address"));
    field(ui, &mut draft.url, "http://127.0.0.1:11434/v1");
    ui.add_space(6.0);
    ui.label(legend("model"));
    field(ui, &mut draft.model, "the model to ask for");
    ui.add_space(6.0);
    ui.label(legend("key"));
    secret(ui, &mut draft.key);
    hint(ui, "Sent as a bearer token. Leave empty for a server that wants none.");
    ui.add_space(6.0);
    ui.label(legend("tool calls per question"));
    let mut steps = draft.steps as f64;
    if ui.add(egui::DragValue::new(&mut steps).range(1.0..=100.0).speed(0.2)).changed() {
        draft.steps = steps as usize;
    }
    ui.add_space(6.0);
    ui.label(legend("workspace folder"));
    let default = Config { workspace: String::new(), ..draft.clone() }.workspace_path(app);
    field(ui, &mut draft.workspace, &default.display().to_string());
    hint(
        ui,
        "Where the agent reads and writes files and runs the tools' command lines. It cannot \
         leave this folder. Empty means the default shown.",
    );
    ui.add_space(6.0);
    ui.label(legend("sampling"));
    optional(ui, "temperature", &mut draft.sampling.temperature);
    optional(ui, "top_p", &mut draft.sampling.top_p);
    optional(ui, "top_k", &mut draft.sampling.top_k);
    optional(ui, "max_tokens", &mut draft.sampling.max_tokens);
    hint(ui, "Leave empty to use the server's own default.");
    ui.add_space(6.0);
    ui.label(legend("extra instructions"));
    prose(ui, &mut draft.brief, "anything to add to what the model is told", 3);
    ui.add_space(12.0);
    hint(ui, &format!("Saved in {}, readable by you only.", where_saved(dir)));
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui.add(Key::new("save").solid(READOUT).min_width(90.0)).clicked() {
            done = Some(Done::Save(draft.clone()));
        }
        if ui.add(Key::new("cancel").outline(VALUE).min_width(90.0)).clicked() {
            done = Some(Done::Cancel);
        }
    });
    done
}

fn optional<T: std::str::FromStr + ToString>(ui: &mut Ui, name: &str, value: &mut Option<T>) {
    let id = ui.id().with(("sampling", name));
    let shown = value.as_ref().map(T::to_string).unwrap_or_default();
    let mut text = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| shown.clone());
    if text.trim().parse::<T>().ok().map(|v| v.to_string()) != value.as_ref().map(T::to_string)
        && !(text.trim().is_empty() && value.is_none())
    {
        text = shown;
    }
    ui.horizontal(|ui| {
        ui.label(name);
        field(ui, &mut text, "default");
    });
    *value = text.trim().parse().ok();
    ui.data_mut(|d| d.insert_temp(id, text));
}

fn where_saved(dir: Option<&Path>) -> String {
    Config::path(dir).map(|p| p.display().to_string()).unwrap_or_else(|| "nowhere".into())
}
