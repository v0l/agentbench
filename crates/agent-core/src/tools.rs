use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub about: String,
    pub schema: Value,
}

impl Spec {
    pub fn new(name: &str, about: &str, schema: Value) -> Self {
        Self { name: name.to_string(), about: about.to_string(), schema }
    }
}

pub fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

pub fn string(about: &str) -> Value {
    json!({ "type": "string", "description": about })
}

pub fn integer(about: &str) -> Value {
    json!({ "type": "integer", "description": about })
}

pub fn boolean(about: &str) -> Value {
    json!({ "type": "boolean", "description": about })
}

pub fn text<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str))
}

pub fn number(args: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .find_map(|k| {
            let v = args.get(*k)?;
            v.as_u64()
                .or_else(|| v.as_f64().map(|f| f as u64))
                .or_else(|| v.as_str()?.trim().parse().ok())
        })
        .map(|n| n as usize)
}

pub fn flag(args: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| args.get(*k)?.as_bool())
}

pub const PATH: [&str; 4] = ["path", "file_path", "filePath", "file"];

pub fn builtin() -> Vec<Spec> {
    let path = string("A path inside the workspace, relative to it");
    let mut specs = vec![
        Spec::new(
            "load_skill",
            "Read an agent skill: the manual for a tool, with its file formats and workflow. \
             Load the skill before writing or editing that tool's files. With no name it lists \
             the skills. A skill may have other files, which `file` reads.",
            object(
                json!({
                    "name": string("The skill to load"),
                    "file": string("Another file in the skill, named when you loaded it"),
                }),
                &[],
            ),
        ),
        Spec::new("workspace", "Say which folder the file tools work in.", object(json!({}), &[])),
        Spec::new(
            "request_access",
            "Ask the user to share a folder or file outside the workspace, when the work needs \
             it: a project folder, a downloads folder, a parts library. They see the path and your \
             reason and choose read only, read and write, or no. Ask for the narrowest folder that \
             will do, and ask for read only unless you must change something. Once it is shared \
             the file tools take its absolute path, and bash can read it, and write to it if write \
             was granted, until the user clears the chat or stops sharing. Credential folders and \
             the whole home folder are refused without asking.",
            object(
                json!({
                    "path": string("The folder or file to share, absolute or starting with ~"),
                    "write": json!({ "type": "boolean", "description": "Whether you need to change it, default false" }),
                    "reason": string("One sentence the user reads when deciding"),
                }),
                &["path", "reason"],
            ),
        ),
        Spec::new(
            "list_files",
            "List the files and folders in the workspace, skipping .git and target.",
            object(
                json!({
                    "path": path,
                    "depth": integer("How many levels down, 1 to 6, default 2"),
                }),
                &[],
            ),
        ),
        Spec::new(
            "read_file",
            "Read a text file in the workspace with a line budget. A long file is read in \
             pages: the answer says which offset to ask for next.",
            object(
                json!({
                    "path": path,
                    "offset": integer("The line to start at, from 1"),
                    "limit": integer("How many lines"),
                }),
                &["path"],
            ),
        ),
        Spec::new(
            "write_file",
            "Create a file in the workspace, or replace one whole. Folders are made as needed. \
             To change part of a file use edit_file.",
            object(
                json!({ "path": path, "content": string("The whole file") }),
                &["path", "content"],
            ),
        ),
        Spec::new(
            "edit_file",
            "Replace text in a file in the workspace. `old` must match the file exactly and \
             appear once, unless all is true. Read the file first.",
            object(
                json!({
                    "path": path,
                    "old": string("The exact text to replace, with enough lines round it to be unique"),
                    "new": string("What to put there"),
                    "all": { "type": "boolean", "description": "Replace every occurrence" },
                }),
                &["path", "old", "new"],
            ),
        ),
        Spec::new(
            "search_files",
            "Find lines in the workspace's text files that contain some text, ignoring case.",
            object(
                json!({
                    "pattern": string("The text to look for"),
                    "path": string("A folder or file to search in, default the whole workspace"),
                }),
                &["pattern"],
            ),
        ),
        Spec::new(
            "bash",
            "Run a shell command in the workspace and read what it prints: pipes, globs, \
             redirects, python, git, make, anything installed. It runs in an OS sandbox: it can \
             read the system and the installed tools, and write only inside the workspace and a \
             scratch folder. The rest of the user's files, such as their home folder, are not \
             readable. The network is open. Use the file tools for editing; use this for \
             everything else.",
            object(
                json!({
                    "command": string("The command line, run by bash -c"),
                    "timeout_s": integer("Seconds before it is killed, default 120, at most 900"),
                    "background": boolean("Start it and answer at once with a handle, for a dev server, a watcher or a long build. Read it with bash_output and stop it with bash_kill. Needs no timeout"),
                }),
                &["command"],
            ),
        ),
        Spec::new(
            "bash_output",
            "Read what a background command has printed since you last read it, and whether it \
             is still running or how it exited. With no handle it lists your background commands.",
            object(
                json!({
                    "handle": string("The handle bash gave, like job-1"),
                    "wait_s": integer("Wait up to this many seconds for new output when there is none, default 0, at most 60"),
                }),
                &[],
            ),
        ),
        Spec::new(
            "bash_kill",
            "Stop a background command and everything it started.",
            object(json!({ "handle": string("The handle bash gave, like job-1") }), &["handle"]),
        ),
        Spec::new(
            "todo_write",
            "Keep a checklist of the steps of a long task, shown to the user in the chat. Send the \
             whole list each time: each item has `text` and a `status` of pending, in_progress or \
             done. Keep one item in progress. Use it for work of three or more steps.",
            object(
                json!({
                    "items": {
                        "type": "array",
                        "description": "The whole checklist, in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": string("The step"),
                                "status": { "type": "string", "enum": ["pending", "in_progress", "done"] },
                            },
                            "required": ["text"],
                        },
                    },
                }),
                &["items"],
            ),
        ),
        Spec::new(
            "web_search",
            "Search the web and get back titles, addresses and snippets. Follow up the promising \
             results with web_fetch. Use it for current facts, documentation, error messages and \
             anything outside the workspace.",
            object(
                json!({
                    "query": string("What to search for"),
                    "limit": integer("How many results, default 8, at most 20"),
                    "domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Only results from these sites, e.g. [\"docs.rs\"]; prefix one with - to leave it out, e.g. [\"-pinterest.com\"]",
                    },
                }),
                &["query"],
            ),
        ),
        Spec::new(
            "web_fetch",
            "Fetch a web address. A web page comes back as readable markdown with the menus, \
             scripts and footers removed, and its links kept. Long pages are cut: the answer says \
             which offset to ask for next, so read on only if you need to. Text such as JSON or \
             plain files comes back as it is. Pass save_to to download any file (a zip, a picture, \
             a PDF) to disk instead, then use read_file or bash on it. Pass prompt, a request such \
             as \"list the breaking changes\", to get just that answer from the page, written by a \
             small fast model, instead of the page itself, when the app has one.",
            object(
                json!({
                    "url": string("An http or https address"),
                    "offset": integer("The character to start at, to read on from a cut page"),
                    "max_chars": integer("How many characters to return, default 20000, at most 100000"),
                    "raw": boolean("Return the HTML source instead of markdown"),
                    "prompt": string("What to extract from the page, e.g. \"summarise this\"; returns the answer, not the page"),
                    "save_to": string("Save the response body to this path (a folder ending in / keeps the file's own name) instead of showing it"),
                }),
                &["url"],
            ),
        ),
        Spec::new(
            "find_skills",
            "Search the public skills directory (skills.sh) for a skill that covers a task you \
             lack the know-how for, such as a file format, a service or a workflow. The skills \
             already installed are read with load_skill: look there first. Each result gives the \
             skill, its install count, its page and the command that installs it. Check the page \
             and the author before trusting one, and ask the user before you install anything.",
            object(
                json!({
                    "query": string("What the skill should help with, e.g. \"pdf forms\" or \"changelog\""),
                    "limit": integer("How many results, default 8, at most 20"),
                }),
                &["query"],
            ),
        ),
    ];
    if !cfg!(feature = "web") {
        specs.retain(|s| !matches!(s.name.as_str(), "web_search" | "web_fetch"));
    }
    specs.extend(super::code::specs());
    specs
}

pub const GUIDE: &str = "\
- Before writing or editing files in a format you were not taught, load its skill with \
load_skill. A skill is the manual: file formats, workflow and the mistakes to avoid. Do not write \
these formats from memory. load_skill with no name lists the skills, and a skill can have other \
files you read with its file argument.\n\
- Work in the workspace with list_files, read_file, search_files, write_file and edit_file. Read a \
file before you edit it. Paths are relative to the workspace. To use a folder outside it, such as a \
project or a library, call request_access with the folder and a reason: the user chooses read only, \
read and write, or no. Ask for the narrowest folder, and for read only unless you must change \
something.\n\
- bash runs a shell command in a sandbox: it reads the system and the installed tools, writes \
only inside the workspace and the folders the user shared for writing, and cannot see the rest of \
the user's files. Use it for pipes, globs, scripts and anything the other tools do not cover. Pass \
background for a server or a watcher, then read it with bash_output and stop it with bash_kill. It \
is not offered on a machine with no sandbox.\n\
- For code, code_outline shows what is in a file or folder, code_find gives a unit's source and \
everything that uses it, and code_diagnostics says what the language server finds wrong. Prefer \
them to reading or searching whole files.\n\
- web_search and web_fetch look things up. find_skills searches the public skills directory when \
no installed skill covers the job; ask the user before installing one.\n\
- For work of three or more steps keep a checklist with todo_write.\n\
- Write markdown, and put code and file contents in fenced blocks.";

pub const NEEDS_SANDBOX: [&str; 6] =
    ["bash", "bash_output", "bash_kill", "code_outline", "code_find", "code_diagnostics"];

pub fn offered(backend: &dyn super::Backend) -> Vec<Spec> {
    let mut specs = builtin();
    let left_out = backend.leave_out();
    specs.retain(|s| !left_out.contains(&s.name.as_str()));
    if !super::sandbox::supported() {
        specs.retain(|s| !NEEDS_SANDBOX.contains(&s.name.as_str()));
    }
    specs.extend(backend.specs());
    specs
}

pub fn list(backend: &dyn super::Backend) -> Value {
    Value::Array(
        offered(backend)
            .into_iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.about,
                        "parameters": t.schema,
                    },
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_has_a_name_a_sentence_and_an_object_schema() {
        for t in builtin() {
            assert!(t.about.len() > 10, "{}", t.name);
            assert_eq!(t.schema["type"], "object", "{}", t.name);
            assert!(
                super::super::local::NAMES.contains(&t.name.as_str()),
                "{} goes nowhere",
                t.name
            );
        }
        for name in super::super::local::NAMES {
            assert!(builtin().iter().any(|t| t.name == name), "{name} is not offered");
        }
    }
}
