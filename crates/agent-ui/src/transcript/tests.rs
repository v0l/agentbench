use super::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

#[test]
fn a_checklist_is_drawn_as_a_list_with_its_progress() {
    let input = json!({"items": [
        {"text": "read", "status": "done"},
        {"text": "write", "status": "in_progress"},
        {"text": "test"},
    ]});
    assert_eq!(tool_body("todo_write", &input).unwrap(), "[x] read\n[~] write\n[ ] test");
    assert_eq!(tool_summary("todo_write", &input), "1 of 3 done");
}

#[test]
fn claudes_checklist_is_drawn_too() {
    let input = json!({"todos": [
        {"content": "read", "status": "completed"},
        {"content": "write", "status": "pending"},
    ]});
    assert_eq!(tool_body("TodoWrite", &input).unwrap(), "[x] read\n[ ] write");
    assert_eq!(tool_summary("TodoWrite", &input), "1 of 2 done");
}

#[test]
fn tool_summaries_pick_the_telling_field() {
    assert_eq!(tool_summary("bash", &json!({"command": "ls -la\necho"})), "ls -la");
    assert_eq!(tool_summary("read", &json!({"path": "/a/b.rs"})), "/a/b.rs");
    assert_eq!(tool_summary("x", &Value::Null), "");
}

#[test]
fn an_edit_is_shown_as_a_diff_and_a_command_has_no_body() {
    let edit = json!({"path": "a.rs", "old": "a\nb", "new": "c"});
    assert_eq!(tool_body("edit_file", &edit).unwrap(), "- a\n- b\n+ c\n");
    assert!(tool_body("bash", &json!({"command": "ls", "timeout": 3})).is_none());
    assert!(tool_body("read_file", &json!({"path": "a", "offset": 2})).unwrap().contains('\n'));
}

#[test]
fn an_excerpt_stops_at_forty_lines_and_counts_the_rest() {
    let text = (0..100).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
    let (shown, more) = excerpt(&text);
    assert_eq!((shown.lines().count(), more), (40, 60));
    assert_eq!(excerpt("a\nb"), ("a\nb".to_string(), 0));
}

#[test]
fn a_json_answer_is_laid_out_and_text_is_left_alone() {
    assert!(pretty("{\"a\":1}").contains('\n'));
    assert_eq!(pretty("$ gcad check\nok"), "$ gcad check\nok");
}

#[test]
fn the_first_line_skips_blank_ones() {
    assert_eq!(first_line("\n  \n  #680 Server tab\nmore"), "#680 Server tab");
    assert_eq!(first_line(""), "");
}

struct Rig {
    md: markdown::Cache,
    items: Vec<Owned>,
    actions: Vec<(usize, Action)>,
    thinking: bool,
}

enum Owned {
    You { text: String, from: Vec<String>, files: Vec<(i64, String, String, u64)> },
    Said(String),
    Thinking(String),
    Tool { name: String, input: Value, output: Option<String>, error: bool },
    Questions { id: String, questions: Vec<(String, Vec<String>, bool)>, answers: Option<Vec<String>> },
    Error(String),
    Notice(String),
    Warning(String),
    File { caption: String, file: (i64, String, String, u64) },
    Summary(String, String),
}

fn draw(ui: &mut Ui, rig: &mut Rig) {
    let Rig { md, items, actions, thinking } = rig;
    let mut show = Show {
        markdown: md,
        files: &mut NoFiles,
        now: 120_000,
        thinking: *thinking,
        reading: None,
        read_aloud: true,
        save: true,
    };
    for (n, o) in items.iter().enumerate() {
        let id = n as i64 + 1;
        let file_of = |f: &(i64, String, String, u64)| -> (i64, String, String, u64) { f.clone() };
        let done = match o {
            Owned::You { text, from, files } => {
                let files: Vec<File> = files
                    .iter()
                    .map(|(id, name, mime, size)| File { id: *id, name, mime, size: *size })
                    .collect();
                entry(ui, &mut show, id, &Item::You { text, files: &files, from, at: Some(0), queued: false })
            }
            Owned::Said(text) => entry(ui, &mut show, id, &Item::Said { text }),
            Owned::Thinking(text) => entry(ui, &mut show, id, &Item::Thinking { text }),
            Owned::Tool { name, input, output, error } => entry(
                ui,
                &mut show,
                id,
                &Item::Tool { name, input, output: output.as_deref(), error: *error, summary: None },
            ),
            Owned::Questions { id: qid, questions, answers } => {
                let qs: Vec<Question> = questions
                    .iter()
                    .map(|(text, options, multiple)| Question {
                        text,
                        options,
                        multiple: *multiple,
                        secret: false,
                    })
                    .collect();
                entry(ui, &mut show, id, &Item::Questions { id: qid, questions: &qs, answers: answers.as_deref() })
            }
            Owned::Error(text) => entry(ui, &mut show, id, &Item::Error { text }),
            Owned::Notice(text) => entry(ui, &mut show, id, &Item::Notice { text }),
            Owned::Warning(text) => entry(ui, &mut show, id, &Item::Warning { text }),
            Owned::File { caption, file } => {
                let (fid, name, mime, size) = file_of(file);
                entry(
                    ui,
                    &mut show,
                    id,
                    &Item::File { caption, file: File { id: fid, name: &name, mime: &mime, size } },
                )
            }
            Owned::Summary(title, text) => entry(ui, &mut show, id, &Item::Summary { title, text }),
        };
        if let Some(a) = done {
            actions.push((n, a));
        }
    }
}

fn harness(items: Vec<Owned>) -> Harness<'static, Rig> {
    let rig = Rig { md: markdown::Cache::default(), items, actions: Vec::new(), thinking: true };
    let mut h = Harness::builder().with_size([700.0, 900.0]).build_ui_state(
        |ui, rig: &mut Rig| {
            egui_bench::theme::install(ui.ctx());
            egui::ScrollArea::vertical().show(ui, |ui| draw(ui, rig));
        },
        rig,
    );
    h.run_steps(3);
    h
}

fn painted(h: &Harness<'_, Rig>) -> String {
    painted_text(&h.output().shapes)
}

fn painted_text(shapes: &[egui::epaint::ClippedShape]) -> String {
    fn walk(s: &egui::Shape, out: &mut String) {
        match s {
            egui::Shape::Text(t) => {
                out.push_str(t.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    for c in shapes {
        walk(&c.shape, &mut out);
    }
    out
}

fn has(h: &Harness<'_, Rig>, text: &str) -> bool {
    h.query_by_label_contains(text).is_some()
        || painted(h).to_lowercase().contains(&text.to_lowercase())
}

#[test]
fn a_harness_message_is_folded_until_clicked_and_a_typed_one_never_is() {
    let judge = "#680 Server tab per-request table\n\nThe worker says it is done.\n\nSECOND PARAGRAPH";
    let mut h = harness(vec![
        Owned::You { text: judge.into(), from: vec!["judge".into()], files: vec![] },
        Owned::You { text: "please fix the login\n\nTYPED BODY".into(), from: vec![], files: vec![] },
    ]);
    assert!(has(&h, "#680 Server tab per-request table"), "the first line shows folded");
    assert!(!has(&h, "SECOND PARAGRAPH"), "the rest is hidden folded");
    assert!(has(&h, "TYPED BODY"), "a typed message is shown whole");
    h.get_by_label("from judge").click();
    h.run_steps(3);
    assert!(has(&h, "SECOND PARAGRAPH"), "a click opens it");
    assert!(has(&h, "TYPED BODY"));
    h.get_by_label("from judge").click();
    h.run_steps(3);
    assert!(!has(&h, "SECOND PARAGRAPH"), "a second click folds it again");
}

#[test]
fn each_entry_kind_draws_its_words() {
    let h = harness(vec![
        Owned::You {
            text: "look at this".into(),
            from: vec![],
            files: vec![(1, "notes.pdf".into(), "application/pdf".into(), 2048)],
        },
        Owned::Thinking("REASONING about credits".into()),
        Owned::Said("the **answer** is here".into()),
        Owned::Tool {
            name: "bash".into(),
            input: json!({"command": "cargo test -p billing"}),
            output: Some("error[E0425]\nmore".into()),
            error: true,
        },
        Owned::Tool {
            name: "read_file".into(),
            input: json!({"path": "src/billing.rs"}),
            output: None,
            error: false,
        },
        Owned::Error("the model went away".into()),
        Owned::Notice("compacting soon".into()),
        Owned::Warning("context is nearly full".into()),
        Owned::File {
            caption: "the report".into(),
            file: (2, "report.csv".into(), "application/octet-stream".into(), 10),
        },
        Owned::Summary("compacted: the agent carries on".into(), "SUMMARY TEXT".into()),
    ]);
    for words in [
        "look at this",
        "notes.pdf 2 KB",
        "REASONING about credits",
        "answer",
        "read aloud",
        "$ cargo test -p billing",
        "failed",
        "src/billing.rs",
        "running",
        "the model went away",
        "compacting soon",
        "context is nearly full",
        "report.csv",
        "the report",
        "SUMMARY TEXT",
    ] {
        assert!(has(&h, words), "{words} is drawn");
    }
}

#[test]
fn thinking_hides_when_turned_off() {
    let mut h = harness(vec![Owned::Thinking("REASONING".into())]);
    assert!(has(&h, "REASONING"));
    h.state_mut().thinking = false;
    h.run_steps(2);
    assert!(!has(&h, "REASONING"));
}

#[test]
fn a_tool_row_opens_to_its_body_and_output() {
    let mut h = harness(vec![Owned::Tool {
        name: "edit_file".into(),
        input: json!({"path": "a.rs", "old": "OLD LINE", "new": "NEW LINE"}),
        output: Some("edited a.rs".into()),
        error: false,
    }]);
    assert!(!has(&h, "- OLD LINE"));
    h.get_by_label("edit_file").click();
    h.run_steps(3);
    assert!(has(&h, "- OLD LINE"));
    assert!(has(&h, "+ NEW LINE"));
}

#[test]
fn a_single_choice_question_is_answered_by_its_option() {
    let mut h = harness(vec![Owned::Questions {
        id: "q1".into(),
        questions: vec![("Which way?".into(), vec!["left".into(), "right".into()], false)],
        answers: None,
    }]);
    assert!(has(&h, "Which way?"));
    h.get_by_label("right").click();
    h.run_steps(2);
    assert_eq!(h.state().actions, vec![(0, Action::Answer(vec!["right".into()]))]);
}

#[test]
fn several_questions_go_together_once_each_has_an_answer() {
    let mut h = harness(vec![Owned::Questions {
        id: "q2".into(),
        questions: vec![
            ("Which colours?".into(), vec!["red".into(), "blue".into(), "green".into()], true),
            ("Which size?".into(), vec!["small".into(), "large".into()], false),
        ],
        answers: None,
    }]);
    h.get_by_label("red").click();
    h.run_steps(1);
    h.get_by_label("green").click();
    h.run_steps(1);
    h.get_by_label("send answers").click();
    h.run_steps(1);
    assert!(h.state().actions.is_empty(), "not every question has an answer yet");
    h.get_by_label("large").click();
    h.run_steps(1);
    h.get_by_label("send answers").click();
    h.run_steps(2);
    assert_eq!(
        h.state().actions,
        vec![(0, Action::Answer(vec!["red, green".into(), "large".into()]))]
    );
}

#[test]
fn an_answered_question_shows_what_was_said() {
    let h = harness(vec![Owned::Questions {
        id: "q3".into(),
        questions: vec![("Ready?".into(), vec!["yes".into()], false)],
        answers: Some(vec!["yes please".into()]),
    }]);
    assert!(has(&h, "yes please"));
    assert!(h.query_by_label("answer").is_none());
}

#[test]
fn a_reply_can_be_read_aloud_and_a_file_saved() {
    let mut h = harness(vec![
        Owned::Said("some reply".into()),
        Owned::File { caption: String::new(), file: (3, "a.zip".into(), "application/zip".into(), 1) },
    ]);
    h.get_by_label_contains("read aloud").click();
    h.run_steps(1);
    h.get_by_label("save").click();
    h.run_steps(1);
    assert_eq!(h.state().actions, vec![(0, Action::Read), (1, Action::Save)]);
}

#[test]
fn the_plan_folds_to_the_step_in_hand() {
    let plan = vec![
        (Check::Done, "read the code".to_string()),
        (Check::Doing, "WRITE THE FIX".to_string()),
        (Check::Todo, "run the tests".to_string()),
    ];
    let mut h = Harness::builder().with_size([600.0, 300.0]).build_ui_state(
        |ui, plan: &mut Vec<(Check, String)>| {
            egui_bench::theme::install(ui.ctx());
            plan_panel(ui, "s", plan, false)
        },
        plan,
    );
    h.run_steps(2);
    let shown = |h: &Harness<'_, Vec<(Check, String)>>, t: &str| {
        painted_text(&h.output().shapes).to_lowercase().contains(&t.to_lowercase())
    };
    assert!(shown(&h, "1 of 3 done"));
    assert!(shown(&h, "WRITE THE FIX"));
    assert!(!shown(&h, "run the tests"));
    h.get_by_label("plan").click();
    h.run_steps(2);
    assert!(shown(&h, "run the tests"));
}
