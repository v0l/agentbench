use crate::memory::{CHANGED, Memory, Scope};
use crate::runner::Runner;
use crate::store::Store;
use crate::titles::{Target, ask, without_thinking};
use agentbench_core::{Id, MemoryCompaction, MemoryScopeReport, MemorySettings};
use regex::{Captures, Regex};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

pub const SETTING: &str = "memory_settings";
const PATIENCE: Duration = Duration::from_secs(300);
const ATTEMPTS: usize = 4;
const MIN_SAVING_PERCENT: usize = 5;
const MERGE_COVERAGE_PERCENT: usize = 55;
pub const POINTER: &str = "[secret removed: keep it in ~/.agents/secrets/]";

pub const INSTRUCTIONS: &str = "You compact the notes an agent keeps about one project, so every later session reads \
less and still knows everything that is true. Rewrite the notes below as a shorter set of notes.\n\n\
KEEP every fact that is still current: every host, address, path, file name, command, flag, port, version, trap, \
gotcha, decision, preference, rule and measured number. Keep the exact wording of commands, paths and names. \
Keep the user's own words where a note quotes a decision or preference.\n\n\
MERGE notes that say the same thing, or that cover one subject, into one note, and say which notes went into it.\n\n\
DROP history and narration (how something was found out, what was tried first, dated progress logs), logs of what \
was tried and failed when the lesson is already stated, anything marked done, fixed, resolved or superseded, \
repeated sentences, filler and pleasantries.\n\n\
NEVER invent. Do not add a fact, a number, a reason or a command that the notes do not state. Do not guess a \
meaning. If two notes disagree, keep the newer-looking one only when the notes say which is newer, otherwise keep both. \
When unsure whether something is still true, keep it.\n\n\
NEVER copy a password, token, API key, private key or other secret into your answer. Write a pointer to where it is \
kept instead, such as ~/.agents/secrets/.\n\n\
A note must not get longer than it was. Plain markdown, terse sentences, no headings unless the note already has them, \
no emphasis, no dashes used as punctuation.\n\n\
Do not drop a whole note unless every fact in it is finished, fixed, superseded or already stated in another note; \
when in doubt keep it. Small notes that are already terse should come back unchanged.\n\n\
Answer in exactly this format and nothing else, with no preface and no code fence. Each note you keep starts with a line:\n\
=== NOTE: <topic> ===\n\
or, when it merges others:\n\
=== NOTE: <topic> | from: <topic one>, <topic two> ===\n\
followed by the body of the note. Reuse the existing topic name for a note that keeps its identity. Every topic you \
were given must be accounted for exactly once: kept under its own name, listed in a `from:` list, or dropped with a line:\n\
=== DROP: <topic> | <the reason, in a few words> ===\n\
A DROP line has no body.";

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^=== (NOTE|DROP): (.+?)(?: \| (?:from: )?(.+?))? ===[ \t]*$").unwrap()
});
static STRAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^===").unwrap());

static LABELLED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\b(?:password|passwd|secret|token|api[_ -]?key|private[_ -]?key)s?\s*[:=]\s*)(\S{8,})").unwrap()
});
static PREFIXED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|nsec1[0-9a-z]{20,}|glpat-[A-Za-z0-9_-]{16,})").unwrap()
});
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)").unwrap()
});
static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}").unwrap());
static LONG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9_-]{32,}\b").unwrap());

#[derive(Debug, PartialEq)]
struct Rewritten {
    topic: String,
    from: Vec<String>,
    body: String,
}

#[derive(Debug, PartialEq)]
struct Answer {
    notes: Vec<Rewritten>,
    dropped: Vec<(String, String)>,
}

pub fn scrub(text: &str) -> (String, usize) {
    let mut found = 0;
    let mut out = text.to_string();
    out = PRIVATE_KEY
        .replace_all(&out, |_: &Captures| {
            found += 1;
            POINTER.to_string()
        })
        .into_owned();
    out = LABELLED
        .replace_all(&out, |c: &Captures| {
            let value = c[2].trim_matches(['`', '"', '\'']);
            let pointer =
                value.starts_with(['~', '/', '[', '<', '$']) || value.contains("secrets/") || value.starts_with("http");
            if pointer {
                c[0].to_string()
            } else {
                found += 1;
                format!("{}{POINTER}", &c[1])
            }
        })
        .into_owned();
    for re in [&*PREFIXED, &*BEARER] {
        out = re
            .replace_all(&out, |_: &Captures| {
                found += 1;
                POINTER.to_string()
            })
            .into_owned();
    }
    out = LONG
        .replace_all(&out, |c: &Captures| {
            let s = &c[0];
            let mixed = s.chars().any(|ch| ch.is_ascii_uppercase())
                && s.chars().any(|ch| ch.is_ascii_lowercase())
                && s.chars().any(|ch| ch.is_ascii_digit());
            if mixed {
                found += 1;
                POINTER.to_string()
            } else {
                s.to_string()
            }
        })
        .into_owned();
    (out, found)
}

fn identifiers(text: &str, broad: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut ticks = text.split('`');
    ticks.next();
    for (i, span) in ticks.enumerate() {
        if i % 2 == 0 && !span.trim().is_empty() && span.len() < 120 {
            out.push(span.trim().to_string());
        }
    }
    for raw in text.split_whitespace() {
        let t = raw.trim_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '~' | ':' | '_' | '.' | '-' | '$')));
        let t = t.trim_end_matches(['.', ':', '-']);
        if t.len() < 4 || t.contains('`') {
            continue;
        }
        let has_digit = t.chars().any(|c| c.is_ascii_digit());
        let hard = has_digit || t.contains(['/', '_', '~']) || t.starts_with(':') || t.starts_with('$');
        let word = broad && t.contains('-') && t.chars().all(|c| c.is_ascii_lowercase() || c == '-');
        let plain_number = t.chars().all(|c| c.is_ascii_digit() || c == '.');
        if (hard || word) && !plain_number {
            out.push(t.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn lost(old: &str, new: &str, broad: bool) -> Vec<String> {
    identifiers(old, broad).into_iter().filter(|id| !new.contains(id.as_str())).collect()
}

fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 5)
        .map(|w| w.to_lowercase())
        .collect()
}

fn covered(source: &str, target: &str) -> bool {
    let wanted = words(source);
    if wanted.is_empty() {
        return true;
    }
    let have = words(target);
    wanted.iter().filter(|w| have.contains(*w)).count() * 100 >= wanted.len() * MERGE_COVERAGE_PERCENT
}

fn undo_bloated_merges(clean: &[(String, String)], answer: &mut Answer) {
    let size_of = |topic: &str| clean.iter().find(|(t, _)| t == topic).map(|(_, b)| b.len());
    answer.notes.retain(|n| {
        if size_of(&n.topic).is_some() || n.from.is_empty() {
            return true;
        }
        let sources: usize = n.from.iter().filter_map(|f| size_of(f)).sum();
        n.body.len() <= sources
    });
}

fn restore_lost_facts(clean: &[(String, String)], answer: &mut Answer) {
    let everything = |answer: &Answer| answer.notes.iter().map(|n| n.body.as_str()).collect::<Vec<_>>().join("\n");
    for (topic, body) in clean {
        let dropped = answer.dropped.iter().any(|(t, _)| t == topic);
        let merged_into: Vec<String> = answer
            .notes
            .iter()
            .filter(|n| n.from.contains(topic) && n.topic != *topic)
            .map(|n| n.body.clone())
            .collect();
        let thin = !merged_into.is_empty() && !covered(body, &merged_into.join("\n"));
        if (dropped || !merged_into.is_empty()) && (thin || !lost(body, &everything(answer), true).is_empty()) {
            answer.dropped.retain(|(t, _)| t != topic);
            let mut emptied = Vec::new();
            for n in &mut answer.notes {
                if n.from.contains(topic) {
                    n.from.retain(|f| f != topic);
                    if n.from.is_empty() && !clean.iter().any(|(t, _)| *t == n.topic) {
                        emptied.push(n.topic.clone());
                    }
                }
            }
            answer.notes.retain(|n| !emptied.contains(&n.topic));
            answer.notes.push(Rewritten { topic: topic.clone(), from: Vec::new(), body: body.trim().to_string() });
        }
    }
    for note in &mut answer.notes {
        if let Some((_, was)) = clean.iter().find(|(t, _)| *t == note.topic)
            && !lost(was, &note.body, false).is_empty()
        {
            note.body = was.trim().to_string();
        }
    }
}

fn render_all(notes: &[(String, String)]) -> String {
    notes
        .iter()
        .map(|(t, b)| format!("=== NOTE: {t} ===\n{}\n", b.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse(reply: &str) -> Result<Answer, String> {
    let reply = reply.trim();
    let reply = reply
        .strip_prefix("```")
        .and_then(|r| r.strip_suffix("```"))
        .map(|r| r.trim_start_matches(|c: char| c != '\n').trim())
        .unwrap_or(reply);
    let marks: Vec<_> = MARKER.captures_iter(reply).collect();
    let Some(first) = marks.first() else {
        return Err("the answer has no `=== NOTE: topic ===` line".into());
    };
    if !reply[..first.get(0).map_or(0, |m| m.start())].trim().is_empty() {
        return Err("the answer has text before the first note".into());
    }
    let mut answer = Answer { notes: Vec::new(), dropped: Vec::new() };
    for (i, mark) in marks.iter().enumerate() {
        let whole = mark.get(0).ok_or("bad marker")?;
        let end = marks.get(i + 1).and_then(|m| m.get(0)).map_or(reply.len(), |m| m.start());
        let topic = mark[2].trim().to_string();
        let body = reply[whole.end()..end].trim().to_string();
        if STRAY.is_match(&body) {
            return Err(format!("note `{topic}` holds a broken or cut off `===` line"));
        }
        let seen = answer.notes.iter().any(|n| n.topic == topic) || answer.dropped.iter().any(|(t, _)| *t == topic);
        if topic.is_empty() || seen {
            return Err(format!("topic `{topic}` is empty or appears twice"));
        }
        if &mark[1] == "DROP" {
            let reason = mark.get(3).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            if reason.is_empty() || !body.is_empty() {
                return Err(format!("DROP line for `{topic}` needs a reason and no body"));
            }
            answer.dropped.push((topic, reason));
            continue;
        }
        if body.is_empty() {
            return Err(format!("note `{topic}` has no body"));
        }
        let from = mark
            .get(3)
            .map(|m| m.as_str().split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
            .unwrap_or_default();
        answer.notes.push(Rewritten { topic, from, body });
    }
    Ok(answer)
}

fn check(before: &[(String, String)], answer: &Answer, settings: MemorySettings) -> Result<(), String> {
    let after = &answer.notes;
    let old: usize = before.iter().map(|(_, b)| b.len()).sum();
    let new: usize = after.iter().map(|n| n.body.len()).sum();
    let size_of = |topic: &str| before.iter().find(|(t, _)| t == topic).map(|(_, b)| b.len());
    for note in after {
        match size_of(&note.topic) {
            Some(was) if note.body.len() > was => {
                let grown: Vec<String> = after
                    .iter()
                    .filter_map(|n| {
                        let was = size_of(&n.topic)?;
                        (n.body.len() > was).then(|| format!("`{}` grew from {was} to {} bytes", n.topic, n.body.len()))
                    })
                    .collect();
                return Err(format!("notes must never grow, but {}", grown.join(", ")));
            }
            Some(_) => {}
            None => {
                if note.from.is_empty() {
                    return Err(format!("new note `{}` does not say which notes it merges", note.topic));
                }
                let mut sources = 0;
                for source in &note.from {
                    sources += size_of(source).ok_or_else(|| format!("note `{}` merges unknown note `{source}`", note.topic))?;
                }
                if note.body.len() > sources {
                    return Err(format!("merged note `{}` is longer than the notes it merges", note.topic));
                }
            }
        }
    }
    for (topic, _) in before {
        let kept = after.iter().any(|n| &n.topic == topic);
        let merged = after.iter().any(|n| n.from.contains(topic));
        let dropped = answer.dropped.iter().any(|(t, _)| t == topic);
        if [kept, merged, dropped].iter().filter(|x| **x).count() != 1 {
            return Err(format!("note `{topic}` is not accounted for exactly once (kept, merged by `from:`, or DROP)"));
        }
    }
    if let Some((topic, _)) = answer.dropped.iter().find(|(t, _)| !before.iter().any(|(b, _)| b == t)) {
        return Err(format!("DROP names unknown note `{topic}`"));
    }
    if new >= old {
        return Err(format!("no saving: {old} bytes became {new}"));
    }
    if new > settings.scope_budget && new * 100 > old * (100 - MIN_SAVING_PERCENT) {
        return Err(format!(
            "still {new} bytes over the {} byte budget after saving under {MIN_SAVING_PERCENT}%",
            settings.scope_budget
        ));
    }
    Ok(())
}

fn over_budget(memory: &Memory, scope: Scope, settings: MemorySettings) -> bool {
    let (total, largest) = memory.sizes(scope);
    total > settings.scope_budget || largest > settings.note_budget
}

fn user_message(notes: &[(String, String)], settings: MemorySettings, rejected: Option<&str>) -> String {
    let total: usize = notes.iter().map(|(_, b)| b.len()).sum();
    let sizes: Vec<String> = notes.iter().map(|(t, b)| format!("{t}: {} bytes", b.len())).collect();
    let mut text = format!(
        "These notes take {total} bytes. Aim for each note under {} bytes and all of them together under {} bytes. \
Each note you keep must be shorter than the note it replaces, and a merged note shorter than the notes merged into it. \
Cut at least {} bytes in total. Sizes: {}.\n\n{}",
        settings.note_budget,
        settings.scope_budget,
        (total * MIN_SAVING_PERCENT / 100).max(total.saturating_sub(settings.scope_budget)),
        sizes.join("; "),
        render_all(notes)
    );
    if let Some(why) = rejected {
        text.push_str(&format!("\n\nYour last answer was rejected: {why}. Answer again in the same format."));
    }
    text
}

pub async fn compact_scope<F, Fut>(
    memory: &Memory,
    scope: Scope,
    label: &str,
    settings: MemorySettings,
    force: bool,
    model: &F,
) -> MemoryScopeReport
where
    F: Fn(String, u64) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let key = format!("{scope:?}");
    if !memory.claim(&key) {
        return MemoryScopeReport {
            scope: label.to_string(),
            before: memory.sizes(scope).0,
            after: memory.sizes(scope).0,
            skipped: Some("a compaction is already running".into()),
            ..Default::default()
        };
    }
    let report = compact_claimed(memory, scope, label, settings, force, model).await;
    memory.release(&key);
    report
}

async fn compact_claimed<F, Fut>(
    memory: &Memory,
    scope: Scope,
    label: &str,
    settings: MemorySettings,
    force: bool,
    model: &F,
) -> MemoryScopeReport
where
    F: Fn(String, u64) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let skipped = |why: String, before: usize| MemoryScopeReport {
        scope: label.to_string(),
        before,
        after: before,
        skipped: Some(why),
        ..Default::default()
    };
    let mut rejected: Option<String> = None;
    for _ in 0..ATTEMPTS {
        let current = memory.list(scope);
        let before: usize = current.iter().map(|(_, b)| b.len()).sum();
        if current.is_empty() {
            return skipped("no notes".into(), 0);
        }
        if !force && !over_budget(memory, scope, settings) {
            return skipped("under budget".into(), before);
        }
        let expected: Vec<(String, String)> =
            current.iter().map(|(t, b)| (t.clone(), crate::memory::hash_note(b))).collect();
        let mut redacted = Vec::new();
        let clean: Vec<(String, String)> = current
            .iter()
            .map(|(t, b)| {
                let (body, n) = scrub(b);
                if n > 0 {
                    redacted.push(format!("{t}: {n}"));
                }
                (t.clone(), body)
            })
            .collect();
        let tokens = ((before / 2) as u64).clamp(2000, 16_000);
        let reply = match model(user_message(&clean, settings, rejected.as_deref()), tokens).await {
            Ok(r) => without_thinking(&r),
            Err(e) => return skipped(format!("the model failed: {e}"), before),
        };
        let mut answer = match parse(&reply) {
            Ok(n) => n,
            Err(e) => {
                rejected = Some(e);
                continue;
            }
        };
        for note in &mut answer.notes {
            let (body, n) = scrub(&note.body);
            if n > 0 {
                redacted.push(format!("{}: {n}", note.topic));
                note.body = body;
            }
        }
        let own: Vec<String> = answer.notes.iter().map(|n| n.topic.clone()).collect();
        let mut claimed: Vec<String> = own.clone();
        for note in &mut answer.notes {
            note.from.retain(|f| {
                let fresh = !claimed.contains(f);
                if fresh {
                    claimed.push(f.clone());
                }
                fresh
            });
        }
        answer.dropped.retain(|(t, _)| !claimed.contains(t));
        undo_bloated_merges(&clean, &mut answer);
        for (topic, body) in &clean {
            let named = answer.notes.iter().any(|n| &n.topic == topic || n.from.contains(topic))
                || answer.dropped.iter().any(|(t, _)| t == topic);
            if !named {
                answer.notes.push(Rewritten { topic: topic.clone(), from: Vec::new(), body: body.trim().to_string() });
            }
        }
        for note in &mut answer.notes {
            if let Some((_, was)) = clean.iter().find(|(t, b)| *t == note.topic && note.body.len() > b.len()) {
                note.body = was.trim().to_string();
            }
        }
        restore_lost_facts(&clean, &mut answer);
        if let Err(e) = check(&clean, &answer, settings) {
            rejected = Some(e);
            continue;
        }
        let notes = &answer.notes;
        let written: Vec<(String, String)> = notes.iter().map(|n| (n.topic.clone(), n.body.clone())).collect();
        match memory.replace_all(scope, &expected, &written) {
            Ok(version) => {
                let after: usize = notes.iter().map(|n| n.body.len()).sum();
                let kept: HashSet<&str> = notes.iter().map(|n| n.topic.as_str()).collect();
                let mut merged_into: HashSet<&str> = HashSet::new();
                notes.iter().for_each(|n| n.from.iter().for_each(|f| _ = merged_into.insert(f.as_str())));
                let gone: Vec<&String> = current.iter().map(|(t, _)| t).filter(|t| !kept.contains(t.as_str())).collect();
                let rewritten = notes
                    .iter()
                    .filter(|n| clean.iter().any(|(t, b)| *t == n.topic && b.trim() != n.body.trim()))
                    .map(|n| n.topic.clone())
                    .collect();
                let added = notes
                    .iter()
                    .filter(|n| !current.iter().any(|(t, _)| *t == n.topic))
                    .map(|n| n.topic.clone())
                    .collect();
                return MemoryScopeReport {
                    scope: label.to_string(),
                    before,
                    after,
                    skipped: None,
                    rewritten,
                    added,
                    merged: gone.iter().filter(|t| merged_into.contains(t.as_str())).map(|t| t.to_string()).collect(),
                    dropped: answer
                        .dropped
                        .iter()
                        .map(|(t, why)| format!("{t} ({why})"))
                        .collect(),
                    redacted,
                    version: Some(version),
                };
            }
            Err(e) if e.starts_with(CHANGED) => {
                rejected = None;
                continue;
            }
            Err(e) => return skipped(format!("could not write: {e}"), before),
        }
    }
    let before = memory.sizes(scope).0;
    skipped(rejected.map_or("the notes kept changing while compacting".to_string(), |r| format!("rejected: {r}")), before)
}

pub fn settings(store: &Store) -> MemorySettings {
    store.setting(SETTING).ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_settings(store: &Store, new: MemorySettings) -> Result<MemorySettings, String> {
    let clean = MemorySettings {
        note_budget: new.note_budget.clamp(512, 1 << 20),
        scope_budget: new.scope_budget.clamp(1024, 4 << 20),
        auto: new.auto,
    };
    let text = serde_json::to_string(&clean).map_err(|e| e.to_string())?;
    store.set_setting(SETTING, &text).map_err(|e| e.0)?;
    Ok(clean)
}

pub fn target(runner: &Runner) -> Result<Target, String> {
    let main = runner.titles().ok_or("this server has no model to compact notes with")?.main();
    if main.model.trim().is_empty() || main.url.trim().is_empty() {
        return Err("pick a server and model for the native agent under settings".into());
    }
    Ok(Target { url: main.url.clone(), model: main.model.clone(), key: main.key.clone() })
}

pub fn scopes(store: &Store, project: Option<Id>, global: bool) -> Vec<(Scope, String)> {
    let mut out = Vec::new();
    if let Some(id) = project {
        if let Ok(p) = store.project(id) {
            out.push((Scope::Project(id), p.name));
        }
    } else {
        for p in store.projects().unwrap_or_default() {
            out.push((Scope::Project(p.id), p.name));
        }
    }
    if global {
        out.push((Scope::Global, crate::memory::GLOBAL.to_string()));
    }
    out
}

pub async fn run(
    store: &Store,
    target: &Target,
    scopes: Vec<(Scope, String)>,
    force: bool,
) -> MemoryCompaction {
    let settings = settings(store);
    let model = |user: String, tokens: u64| {
        let target = target.clone();
        async move { ask(&target, INSTRUCTIONS, &user, tokens, PATIENCE).await }
    };
    let mut report = MemoryCompaction::default();
    for (scope, label) in scopes {
        let one = compact_scope(store.memory_store(), scope, &label, settings, force, &model).await;
        if one.skipped.as_deref() == Some("no notes") {
            continue;
        }
        if one.skipped.is_none() {
            let _ = store.log_autopilot(
                match scope {
                    Scope::Project(id) => Some(id),
                    Scope::Global => None,
                },
                None,
                &format!("compacted notes of {label}: {} to {} bytes", one.before, one.after),
            );
        }
        report.scopes.push(one);
    }
    report
}

pub fn after_write(runner: &Runner, scope: Scope) {
    let store = runner.store().clone();
    let settings = settings(&store);
    let memory = store.memory_store();
    if !settings.auto || !over_budget(memory, scope, settings) {
        return;
    }
    let key = format!("{scope:?}");
    let print = memory.fingerprint(scope);
    if memory.already_tried(&key, &print) {
        return;
    }
    let Ok(target) = target(runner) else { return };
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    let label = match scope {
        Scope::Project(id) => store.project(id).map(|p| p.name).unwrap_or_default(),
        Scope::Global => crate::memory::GLOBAL.to_string(),
    };
    let changed = runner.changed.clone();
    handle.spawn(async move {
        let done = run(&store, &target, vec![(scope, label)], false).await;
        let memory = store.memory_store();
        if done.scopes.iter().all(|s| s.skipped.is_some()) {
            memory.tried(&format!("{scope:?}"), memory.fingerprint(scope));
        }
        changed.notify_waiters();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::titles::fake::{completion, server};
    use std::cell::RefCell;

    fn filled() -> (Memory, Scope) {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        memory.set(scope, "demo", "tests", &"run cargo test -p server.\n".repeat(30), None).unwrap();
        memory.set(scope, "demo", "tests again", &"run cargo test -p server.\n".repeat(20), None).unwrap();
        memory.set(scope, "demo", "hosts", "build box is 10.0.0.5, path /srv/build", None).unwrap();
        (memory, scope)
    }

    fn small() -> MemorySettings {
        MemorySettings { note_budget: 400, scope_budget: 900, auto: true }
    }

    fn merged_reply() -> String {
        "=== NOTE: tests | from: tests again ===\nrun cargo test -p server.\n\n=== NOTE: hosts ===\nbuild box is 10.0.0.5, path /srv/build".into()
    }

    async fn go(
        memory: &Memory,
        scope: Scope,
        settings: MemorySettings,
        force: bool,
        replies: Vec<String>,
    ) -> (MemoryScopeReport, usize) {
        let calls = RefCell::new(0usize);
        let queue = RefCell::new(replies);
        let model = |_: String, _: u64| {
            *calls.borrow_mut() += 1;
            let mut q = queue.borrow_mut();
            let out = if q.is_empty() { Err("no more replies".to_string()) } else { Ok(q.remove(0)) };
            async move { out }
        };
        let report = compact_scope(memory, scope, "demo", settings, force, &model).await;
        let n = *calls.borrow();
        (report, n)
    }

    #[tokio::test]
    async fn over_budget_compacts_and_under_budget_does_not() {
        let (memory, scope) = filled();
        let (report, calls) = go(&memory, scope, small(), false, vec![merged_reply()]).await;
        assert_eq!(calls, 1);
        assert_eq!(report.skipped, None, "{report:?}");
        assert!(report.after < report.before);
        assert_eq!(report.merged, ["tests again"]);
        assert_eq!(memory.list(scope).len(), 2);

        let roomy = MemorySettings { note_budget: 4096, scope_budget: 12288, auto: true };
        let (report, calls) = go(&memory, scope, roomy, false, vec![merged_reply()]).await;
        assert_eq!(calls, 0);
        assert_eq!(report.skipped.as_deref(), Some("under budget"));
    }

    #[tokio::test]
    async fn a_single_note_over_its_budget_triggers() {
        let memory = Memory::temporary();
        let scope = Scope::Global;
        memory.set(scope, "", "big", &"x ".repeat(300), None).unwrap();
        let settings = MemorySettings { note_budget: 200, scope_budget: 100_000, auto: true };
        let (report, calls) = go(&memory, scope, settings, false, vec!["=== NOTE: big ===\nx".into()]).await;
        assert_eq!((calls, report.skipped), (1, None));
        assert_eq!(memory.list(scope), [("big".to_string(), "x".to_string())]);
    }

    #[tokio::test]
    async fn a_result_that_grows_a_note_is_rejected_and_nothing_is_written() {
        let (memory, scope) = filled();
        let before = memory.list(scope);
        let more = "more words ".repeat(100);
        let grown = format!(
            "=== NOTE: hosts ===\n{more}\n\n=== NOTE: tests ===\n{more}\n\n=== NOTE: tests again ===\n{more}"
        );
        let (report, calls) = go(&memory, scope, small(), false, vec![grown; 4]).await;
        let why = report.skipped.clone().unwrap_or_default();
        assert_eq!(calls, ATTEMPTS, "{why}");
        let why = report.skipped.unwrap();
        assert!(why.contains("over the") || why.contains("no saving"), "{why}");
        assert_eq!(memory.list(scope), before);
        assert!(memory.versions(scope).is_empty());
    }

    #[tokio::test]
    async fn one_note_that_grows_keeps_its_old_text_while_the_rest_still_shrinks() {
        let (memory, scope) = filled();
        let reply = format!("{}\n\n=== NOTE: hosts ===\n{}", merged_reply().split("\n\n=== NOTE: hosts").next().unwrap(), "grown words ".repeat(20));
        let (report, _) = go(&memory, scope, small(), false, vec![reply]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        let hosts = memory.list(scope).into_iter().find(|(t, _)| t == "hosts").unwrap().1;
        assert_eq!(hosts, "build box is 10.0.0.5, path /srv/build");
    }

    #[test]
    fn a_checked_answer_with_a_grown_note_is_refused() {
        let before = vec![("a".to_string(), "x".repeat(50))];
        let answer = Answer {
            notes: vec![Rewritten { topic: "a".into(), from: vec![], body: "y".repeat(60) }],
            dropped: vec![],
        };
        let settings = MemorySettings { note_budget: 4096, scope_budget: 12288, auto: true };
        assert!(check(&before, &answer, settings).unwrap_err().contains("grew"));
    }

    #[tokio::test]
    async fn a_note_the_model_leaves_out_is_kept_as_it_was() {
        let (memory, scope) = filled();
        let reply = "=== NOTE: tests | from: tests again ===\nrun cargo test -p server.".to_string();
        let (report, _) = go(&memory, scope, small(), false, vec![reply]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        let hosts = memory.list(scope).into_iter().find(|(t, _)| t == "hosts").unwrap().1;
        assert_eq!(hosts, "build box is 10.0.0.5, path /srv/build");
    }

    #[tokio::test]
    async fn a_dropped_note_whose_identifiers_are_nowhere_else_comes_back() {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        memory.set(scope, "demo", "filler", &"some words that say little. ".repeat(40), None).unwrap();
        memory.set(scope, "demo", "route", "start_session model code-x-high for PR reviews, see /srv/review:8001", None).unwrap();
        memory.set(scope, "demo", "note", "keep it short", None).unwrap();
        let reply = "=== NOTE: filler ===\nsome words.\n=== DROP: route | old ===\n=== NOTE: note ===\nkeep it short";
        let (report, _) = go(&memory, scope, small(), false, vec![reply.into()]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        assert!(report.dropped.is_empty());
        let route = memory.list(scope).into_iter().find(|(t, _)| t == "route").unwrap().1;
        assert!(route.contains("code-x-high") && route.contains(":8001"));
    }

    #[tokio::test]
    async fn a_rewrite_that_loses_a_path_or_number_keeps_the_old_text() {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        memory.set(scope, "demo", "filler", &"some words that say little. ".repeat(40), None).unwrap();
        memory.set(scope, "demo", "hosts", "the build box lives at 10.0.0.5 under /srv/build and is slow", None).unwrap();
        let reply = "=== NOTE: filler ===\nsome words.\n=== NOTE: hosts ===\nbuild box is slow";
        let (report, _) = go(&memory, scope, small(), false, vec![reply.into()]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        let hosts = memory.list(scope).into_iter().find(|(t, _)| t == "hosts").unwrap().1;
        assert!(hosts.contains("10.0.0.5") && hosts.contains("/srv/build"));
    }

    #[tokio::test]
    async fn a_note_claimed_as_merged_into_a_note_that_lacks_it_comes_back() {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        memory.set(scope, "demo", "filler", &"some words that say little. ".repeat(40), None).unwrap();
        memory.set(scope, "demo", "other", "Answer questions from the code and never file issues unless asked", None).unwrap();
        memory.set(scope, "demo", "third", "keep it short", None).unwrap();
        let reply = "=== NOTE: filler | from: other ===\nsome words.\n=== NOTE: third ===\nkeep it short";
        let (report, _) = go(&memory, scope, small(), false, vec![reply.into()]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        assert!(report.merged.is_empty());
        let other = memory.list(scope).into_iter().find(|(t, _)| t == "other").unwrap().1;
        assert!(other.contains("never file issues"));
    }

    #[tokio::test]
    async fn a_merged_note_longer_than_its_sources_is_undone_and_the_sources_stay() {
        let (memory, scope) = filled();
        let reply = format!("=== NOTE: tests | from: tests again ===\nrun cargo test -p server.\n=== NOTE: all hosts | from: hosts ===\n{}", "padding words ".repeat(20));
        let (report, _) = go(&memory, scope, small(), false, vec![reply]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        assert!(memory.list(scope).iter().any(|(t, _)| t == "hosts"));
        assert!(!memory.list(scope).iter().any(|(t, _)| t == "all hosts"));
    }

    #[test]
    fn a_dropped_setting_name_is_noticed() {
        let old = "Workers use CARGO_BUILD_JOBS=8 and run long suites. Autopilot cap 8 / project_cap 4.";
        let new = "Workers use CARGO_BUILD_JOBS=8 and run long suites.";
        assert_eq!(lost(old, new, false), ["project_cap"]);
    }

    #[tokio::test]
    async fn a_result_that_does_not_shrink_enough_is_rejected() {
        let (memory, scope) = filled();
        let before = memory.list(scope);
        let same: String = before.iter().map(|(t, b)| format!("=== NOTE: {t} ===\n{b}\n")).collect();
        let (report, _) = go(&memory, scope, small(), false, vec![same; 4]).await;
        assert!(report.skipped.unwrap().contains("no saving"));
        assert_eq!(memory.list(scope), before);
    }

    #[tokio::test]
    async fn a_rejected_answer_is_retried_with_the_reason() {
        let (memory, scope) = filled();
        let (report, calls) =
            go(&memory, scope, small(), false, vec!["I could not do that".into(), merged_reply()]).await;
        assert_eq!((calls, report.skipped), (2, None));
    }

    #[tokio::test]
    async fn an_edit_during_the_model_call_wins_and_the_pass_retries() {
        let (memory, scope) = filled();
        let calls = RefCell::new(0usize);
        let model = |_: String, _: u64| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() == 1 {
                let hash = memory.hash(scope, "hosts");
                memory.set(scope, "demo", "hosts", "build box is 10.0.0.9, path /srv/build", Some(&hash)).unwrap();
            }
            async { Ok("=== NOTE: tests | from: tests again ===\nrun cargo test -p server.\n\n=== NOTE: hosts ===\nbuild box is 10.0.0.9, path /srv/build".to_string()) }
        };
        let report = compact_scope(&memory, scope, "demo", small(), false, &model).await;
        assert_eq!(*calls.borrow(), 2, "{report:?}");
        assert_eq!(report.skipped, None);
        let hosts = memory.list(scope).into_iter().find(|(t, _)| t == "hosts").unwrap().1;
        assert_eq!(hosts, "build box is 10.0.0.9, path /srv/build");
    }

    #[tokio::test]
    async fn an_edit_that_keeps_landing_is_never_clobbered() {
        let (memory, scope) = filled();
        let calls = RefCell::new(0usize);
        let model = |_: String, _: u64| {
            let n = {
                *calls.borrow_mut() += 1;
                *calls.borrow()
            };
            let hash = memory.hash(scope, "hosts");
            let body = format!("build box is 10.0.0.{n}, path /srv/build");
            memory.set(scope, "demo", "hosts", &body, Some(&hash)).unwrap();
            async { Ok("=== NOTE: tests | from: tests again ===\nrun cargo test.\n\n=== NOTE: hosts ===\nstale".to_string()) }
        };
        let report = compact_scope(&memory, scope, "demo", small(), false, &model).await;
        assert!(report.skipped.is_some());
        let hosts = memory.list(scope).into_iter().find(|(t, _)| t == "hosts").unwrap().1;
        assert_eq!(hosts, format!("build box is 10.0.0.{ATTEMPTS}, path /srv/build"));
    }

    #[tokio::test]
    async fn the_version_before_compaction_can_be_restored() {
        let (memory, scope) = filled();
        let before = memory.list(scope);
        let (report, _) = go(&memory, scope, small(), false, vec![merged_reply()]).await;
        let version = report.version.unwrap();
        assert_eq!(memory.versions(scope), std::slice::from_ref(&version));
        assert_ne!(memory.list(scope), before);
        memory.restore(scope, &version).unwrap();
        let mut now = memory.list(scope);
        let mut was = before;
        now.sort();
        was.sort();
        assert_eq!(now, was);
        assert!(memory.restore(scope, "../x").is_err());
    }

    #[tokio::test]
    async fn project_markers_and_other_files_survive() {
        let store = Store::memory().unwrap();
        let (project, _) = crate::store::testing::project(&store, "/a");
        let memory = store.memory_store();
        let scope = Scope::Project(project.id);
        memory.set(scope, "demo", "tests", &"run cargo test -p server.\n".repeat(40), None).unwrap();
        memory.set(scope, "demo", "tests again", &"run cargo test -p server.\n".repeat(40), None).unwrap();
        let top = store.memory_root().join("hand-written.md");
        std::fs::write(&top, "mine").unwrap();
        let dir = std::fs::read_dir(store.memory_root())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.join(".project").exists())
            .unwrap();
        let marker = std::fs::read_to_string(dir.join(".project")).unwrap();
        let reply = "=== NOTE: tests | from: tests again ===\nrun cargo test -p server.".to_string();
        let (report, _) = go(memory, scope, small(), false, vec![reply]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        assert_eq!(std::fs::read_to_string(dir.join(".project")).unwrap(), marker);
        assert_eq!(std::fs::read_to_string(&top).unwrap(), "mine");
        assert_eq!(memory.list(scope).len(), 1);
    }

    #[tokio::test]
    async fn a_secret_is_replaced_by_a_pointer_and_reported() {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        let filler = "run the thing with the usual flags. ".repeat(30);
        let body = format!("{filler}\napi key: sk-abcdefghijklmnopqrstuv123456\npassword: hunter2hunter2");
        memory.set(scope, "demo", "deploy", &body, None).unwrap();
        let leaked = "=== NOTE: deploy ===\nrun the thing. api key: sk-abcdefghijklmnopqrstuv123456 and ghp_abcdefghijklmnopqrstuvwxyz0123 and token=ZZtopSecretValue99";
        let (report, _) = go(&memory, scope, small(), false, vec![leaked.into()]).await;
        assert_eq!(report.skipped, None, "{report:?}");
        assert!(!report.redacted.is_empty());
        let written = memory.list(scope).remove(0).1;
        for secret in ["sk-abcdefghijklmnop", "ghp_abcdef", "ZZtopSecretValue99", "hunter2"] {
            assert!(!written.contains(secret), "{secret} leaked into {written}");
        }
        assert!(written.contains(POINTER));
    }

    #[tokio::test]
    async fn the_secrets_are_not_sent_to_the_model_either() {
        let memory = Memory::temporary();
        let scope = Scope::Project(1);
        let body = format!("{}\npassword: hunter2hunter2", "run the thing with the usual flags. ".repeat(30));
        memory.set(scope, "demo", "deploy", &body, None).unwrap();
        let seen = RefCell::new(String::new());
        let model = |user: String, _: u64| {
            *seen.borrow_mut() = user;
            async { Ok("=== NOTE: deploy ===\nrun it.".to_string()) }
        };
        compact_scope(&memory, scope, "demo", small(), false, &model).await;
        assert!(!seen.borrow().contains("hunter2"));
    }

    #[test]
    fn paths_to_secrets_and_plain_words_are_left_alone() {
        let text = "token: ~/.agents/secrets/github\npassword file at /etc/x\nthe token command, abcdefghijklmnopqrstuvwxyzabcdefgh";
        assert_eq!(scrub(text), (text.to_string(), 0));
    }

    #[test]
    fn the_answer_format_is_strict() {
        assert!(parse("").is_err());
        assert!(parse("sure! here it is").is_err());
        assert!(parse("hello\n=== NOTE: a ===\nbody").is_err());
        assert!(parse("=== NOTE: a ===\n\n=== NOTE: b ===\nbody").is_err());
        assert!(parse("=== NOTE: a ===\nx\n=== NOTE: a ===\ny").is_err());
        assert!(parse("=== NOTE: a ===\nx\n=== DROP: b ===").is_err());
        assert!(parse("=== NOTE: a ===\nx\n=== NOTE: b ===\ny\n=== NO").is_err());
        let ok = parse("```\n=== NOTE: a | from: b, c ===\nx\n\n=== NOTE: d ===\ny\n=== DROP: e | done ===\n```").unwrap();
        assert_eq!(ok.notes[0], Rewritten { topic: "a".into(), from: vec!["b".into(), "c".into()], body: "x".into() });
        assert_eq!(ok.notes[1].body, "y");
        assert_eq!(ok.dropped, [("e".to_string(), "done".to_string())]);
    }

    #[test]
    fn a_new_note_must_name_real_sources() {
        let before = vec![("a".to_string(), "x".repeat(50)), ("b".to_string(), "y".repeat(50))];
        let note = |topic: &str, from: &[&str], n: usize| Rewritten {
            topic: topic.into(),
            from: from.iter().map(|s| s.to_string()).collect(),
            body: "z".repeat(n),
        };
        let settings = MemorySettings { note_budget: 4096, scope_budget: 12288, auto: true };
        let answer = |notes: Vec<Rewritten>| Answer { notes, dropped: Vec::new() };
        assert!(check(&before, &answer(vec![note("c", &[], 10)]), settings).is_err());
        assert!(check(&before, &answer(vec![note("c", &["nope"], 10)]), settings).is_err());
        assert!(check(&before, &answer(vec![note("c", &["a", "b"], 200)]), settings).is_err());
        assert!(check(&before, &answer(vec![note("c", &["a", "b"], 60)]), settings).is_ok());
        assert!(check(&before, &answer(vec![note("a", &[], 10)]), settings).is_err());
        let dropping = Answer { notes: vec![note("a", &[], 10)], dropped: vec![("b".into(), "done".into())] };
        assert!(check(&before, &dropping, settings).is_ok());
        let twice = Answer { notes: vec![note("a", &[], 10)], dropped: vec![("a".into(), "x".into()), ("b".into(), "x".into())] };
        assert!(check(&before, &twice, settings).is_err());
    }

    #[tokio::test]
    async fn the_real_route_asks_the_model_over_http() {
        let (url, seen) = server(vec![(200, completion(&merged_reply()))]);
        let target = Target { url, model: "m".into(), key: String::new() };
        let store = Store::memory().unwrap();
        let (project, _) = crate::store::testing::project(&store, "/a");
        let scope = Scope::Project(project.id);
        let memory = store.memory_store();
        memory.set(scope, "demo", "tests", &"run cargo test -p server.\n".repeat(30), None).unwrap();
        memory.set(scope, "demo", "tests again", &"run cargo test -p server.\n".repeat(20), None).unwrap();
        memory.set(scope, "demo", "hosts", "build box is 10.0.0.5, path /srv/build", None).unwrap();
        save_settings(&store, small()).unwrap();
        let done = run(&store, &target, scopes(&store, Some(project.id), false), false).await;
        assert_eq!(done.scopes.len(), 1);
        assert_eq!(done.scopes[0].skipped, None, "{done:?}");
        let asked = seen.lock().unwrap();
        assert_eq!(asked[0]["messages"][0]["content"], INSTRUCTIONS);
        assert!(asked[0]["messages"][1]["content"].as_str().unwrap().contains("=== NOTE: hosts ==="));
    }
}
