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
    let steps_label = ui.label(legend("tool calls per question"));
    let mut steps = draft.steps as f64;
    let drag = ui.add(egui::DragValue::new(&mut steps).range(1.0..=100.0).speed(0.2)).labelled_by(steps_label.id);
    if drag.changed() {
        draft.steps = steps as usize;
    }
    ui.add_space(6.0);
    ui.label(legend("thinking"));
    choice(
        ui,
        "thinking",
        &mut draft.thinking,
        [
            (None, "server default".to_string()),
            (Some(true), "on".to_string()),
            (Some(false), "off".to_string()),
        ],
    );
    hint(ui, "Sent as chat_template_kwargs.enable_thinking. Server default sends nothing.");
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
    if parsed::<T>(&text).map(|v| v.to_string()).unwrap_or_default() != shown {
        text = shown;
    }
    let before = text.clone();
    ui.horizontal(|ui| {
        ui.label(name);
        field(ui, &mut text, &format!("{name} default"));
    });
    if !text.trim().is_empty() && parsed::<T>(&text).is_none() {
        text = before;
    }
    *value = parsed(&text);
    ui.data_mut(|d| d.insert_temp(id, text));
}

fn parsed<T: std::str::FromStr>(text: &str) -> Option<T> {
    let text = text.trim();
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    text.parse().ok()
}

fn where_saved(dir: Option<&Path>) -> String {
    Config::path(dir).map(|p| p.display().to_string()).unwrap_or_else(|| "nowhere".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn form(size: [f32; 2]) -> Harness<'static, Config> {
        let mut h = Harness::builder().with_size(size).build_ui_state(
            |ui, draft: &mut Config| {
                egui_bench::theme::install(ui.ctx());
                egui::ScrollArea::vertical().show(ui, |ui| {
                    show(ui, draft, "test", None);
                });
            },
            Config::default(),
        );
        h.run_steps(3);
        h
    }

    fn typed(h: &mut Harness<'_, Config>, label: &str, text: &str) {
        for c in text.chars() {
            h.get_by_label(label).click();
            h.run_steps(1);
            h.get_by_label(label).type_text(&c.to_string());
            h.run_steps(2);
        }
    }

    fn backspace(h: &mut Harness<'_, Config>) {
        h.key_press(egui::Key::Backspace);
        h.run_steps(2);
    }

    #[test]
    fn temperature_is_typed_one_character_at_a_time() {
        let mut h = form([600.0, 1400.0]);
        let label = "temperature default";
        typed(&mut h, label, "0");
        assert_eq!(h.state().sampling.temperature, Some(0.0));
        typed(&mut h, label, ".");
        assert_eq!(h.state().sampling.temperature, Some(0.0));
        assert!(h.get_by_label(label).value().unwrap_or_default().ends_with('.'));
        typed(&mut h, label, "6");
        assert_eq!(h.state().sampling.temperature, Some(0.6));
        typed(&mut h, label, "5");
        assert_eq!(h.state().sampling.temperature, Some(0.65));
        typed(&mut h, label, "abc");
        assert_eq!(h.state().sampling.temperature, Some(0.65));
        assert_eq!(h.get_by_label(label).value().as_deref(), Some("0.65"));
        typed(&mut h, label, "-");
        assert_eq!(h.get_by_label(label).value().as_deref(), Some("0.65"));
        for _ in 0..4 {
            backspace(&mut h);
        }
        assert_eq!(h.state().sampling.temperature, None);
    }

    #[test]
    fn top_k_takes_whole_numbers_only() {
        let mut h = form([600.0, 1400.0]);
        let label = "top_k default";
        typed(&mut h, label, "40");
        assert_eq!(h.state().sampling.top_k, Some(40));
        typed(&mut h, label, ".");
        assert_eq!(h.state().sampling.top_k, Some(40));
        assert_eq!(h.get_by_label(label).value().as_deref(), Some("40"));
        backspace(&mut h);
        backspace(&mut h);
        assert_eq!(h.state().sampling.top_k, None);
    }

    #[test]
    fn the_form_with_sampling_fits_a_phone_and_a_desktop() {
        for (name, size) in [("desktop", [1280.0, 800.0]), ("phone", [390.0, 844.0])] {
            let h = form(size);
            for label in ["temperature default", "top_p default", "top_k default", "max_tokens default"] {
                let node = h.get_by_label(label);
                assert!(node.rect().width() > 40.0, "{name} {label}");
                assert!(node.rect().max.x <= size[0] + 1.0, "{name} {label} runs off the right");
            }
        }
    }
}
