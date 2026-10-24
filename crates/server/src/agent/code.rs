//! Code tools for the native agent, built on language servers.
//!
//! `code_outline` breaks a file or folder into its units, `code_find` finds a unit by name and
//! answers with its source and everything that uses it, and `code_diagnostics` says what the
//! language server thinks is wrong. After the agent edits a file, `after_edit` asks the server
//! about it so the errors can be put in front of the model without it having to ask.
//!
//! Nothing here knows a language. Each server is a `[[code.lsp]]` entry in `agentbench.toml`:
//! the command to run, the file extensions it serves and the files that show a project uses it.
//! Servers for Rust, Go, TypeScript, Python and C are set up unless the config says otherwise.
//! A server is started the first time it is needed, one per project folder, and stopped when
//! it has been idle a while.

use super::files;
use super::lsp::{Client, Diag, Found, Pos, Sym, inside};
use agentbench_core::{Lsp, LspHealth};
use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long a server may sit unused before it is stopped.
const IDLE: Duration = Duration::from_secs(20 * 60);
/// Servers kept running at once; the one unused longest goes first.
const MAX_LIVE: usize = 6;
/// How long a tool waits for a server that is still indexing the project.
const INDEXING: Duration = Duration::from_secs(90);
/// How long the check after an edit waits for the same.
const INDEXING_AFTER_EDIT: Duration = Duration::from_secs(40);
const START: Duration = Duration::from_secs(30);
const MATCHES: usize = 8;
const REFS: usize = 40;
const BODY_LINES: usize = 120;
const OUTLINE_LINES: usize = 400;
const OUTLINE_FILES: usize = 60;
const DIAGS_PER_FILE: usize = 20;
const SKIPPED: [&str; 5] = [".git", "target", "node_modules", "dist", "build"];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One language server: how to run it and what it serves.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    /// The program, on PATH or a full path. Empty switches a built-in server off.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions it serves, without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// The language id sent when a file is opened; defaults to the server's name.
    #[serde(default)]
    pub language: String,
    /// Language id by extension, where one server serves several (`tsx = "typescriptreact"`).
    #[serde(default)]
    pub ids: HashMap<String, String>,
    /// Files in the project folder that show the project uses this language. None means any.
    #[serde(default)]
    pub markers: Vec<String>,
    /// Sent to the server as its initialization options.
    #[serde(default)]
    pub options: Option<Value>,
    /// How to install it, said when it is missing.
    #[serde(default)]
    pub install: String,
}

impl ServerSpec {
    fn serves(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        !self.command.trim().is_empty()
            && self.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext))
    }

    fn language_of(&self, path: &Path) -> String {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
        self.ids
            .get(&ext)
            .cloned()
            .or_else(|| Some(self.language.clone()).filter(|l| !l.is_empty()))
            .unwrap_or_else(|| self.name.clone())
    }

    /// Where the server's program is: a full path as given, else the project's own
    /// `node_modules/.bin`, then `PATH`, then the places tools install themselves to, which a
    /// service started by systemd does not have on its `PATH`.
    fn program(&self, project: &Path) -> Option<PathBuf> {
        let cmd = self.command.trim();
        if cmd.is_empty() {
            return None;
        }
        let cmd = Path::new(cmd);
        if cmd.components().count() > 1 {
            return super::resolve(cmd);
        }
        let mut dirs = vec![project.join("node_modules/.bin")];
        dirs.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
        dirs.extend(tool_dirs());
        dirs.into_iter().map(|d| d.join(cmd)).find(|p| p.is_file())
    }

    fn installed(&self, project: &Path) -> bool {
        self.program(project).is_some()
    }

    fn overlaps(&self, other: &ServerSpec) -> bool {
        self.extensions.iter().any(|e| other.extensions.iter().any(|o| o.eq_ignore_ascii_case(e)))
    }
}

/// Folders programs get installed into by cargo, go, npm, pip and friends.
fn tool_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    let mut dirs = vec![
        home.join(".cargo/bin"),
        home.join("go/bin"),
        home.join(".local/bin"),
        home.join(".npm-global/bin"),
        home.join(".bun/bin"),
        home.join(".deno/bin"),
        home.join(".pub-cache/bin"),
        home.join(".ghcup/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/local/go/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    // every node that nvm has installed, newest first
    if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut found: Vec<PathBuf> = versions.flatten().map(|v| v.path().join("bin")).collect();
        found.sort();
        found.reverse();
        dirs.extend(found);
    }
    dirs
}

/// The `[code]` table of `agentbench.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Put the language server's errors in front of the model after it edits a file.
    pub diagnostics: bool,
    /// The role of the message that carries them. `user` works with every server; `developer`
    /// is the role OpenAI's API has for messages from the harness, and many local servers
    /// reject it.
    pub role: String,
    /// Language servers, added to the built-in ones; one with the same name replaces it.
    pub lsp: Vec<ServerSpec>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { diagnostics: true, role: "user".into(), lsp: Vec::new() }
    }
}

fn spec(name: &str, command: &str, args: &[&str], exts: &[&str], markers: &[&str]) -> ServerSpec {
    ServerSpec {
        name: name.into(),
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        extensions: exts.iter().map(|s| s.to_string()).collect(),
        markers: markers.iter().map(|s| s.to_string()).collect(),
        ..ServerSpec::default()
    }
}

pub fn builtin() -> Vec<ServerSpec> {
    let mut rust = spec("rust", "rust-analyzer", &[], &["rs"], &["Cargo.toml"]);
    rust.install = "rustup component add rust-analyzer".into();
    rust.options = Some(json!({
        // the agent's own cargo runs keep the build folder; the server gets its own
        "cargo": { "targetDir": true },
        "checkOnSave": false,
        "workspace": { "symbol": { "search": { "limit": 2048 } } },
    }));
    let mut ts = spec(
        "typescript",
        "typescript-language-server",
        &["--stdio"],
        &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        &["tsconfig.json", "package.json", "jsconfig.json"],
    );
    ts.ids = [
        ("tsx", "typescriptreact"),
        ("js", "javascript"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("jsx", "javascriptreact"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    ts.language = "typescript".into();
    ts.install = "npm install -g typescript-language-server typescript".into();
    let mut c = spec(
        "c",
        "clangd",
        &[],
        &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        &["compile_commands.json", "CMakeLists.txt", "compile_flags.txt"],
    );
    c.ids = [("c", "c"), ("h", "c")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    c.language = "cpp".into();
    c.install = "install clangd with your package manager (apt install clangd)".into();
    let mut python = spec(
        "python",
        "pyright-langserver",
        &["--stdio"],
        &["py", "pyi"],
        &["pyproject.toml", "setup.py", "requirements.txt", "pyrightconfig.json"],
    );
    python.language = "python".into();
    python.install = "npm install -g pyright".into();
    // used when pyright is not installed
    let py_markers = &["pyproject.toml", "setup.py", "requirements.txt", "pyrightconfig.json"];
    let mut pylsp = spec("python-pylsp", "pylsp", &[], &["py", "pyi"], py_markers);
    pylsp.language = "python".into();
    pylsp.install = "pipx install python-lsp-server".into();
    let mut go = spec("go", "gopls", &[], &["go"], &["go.mod", "go.work"]);
    go.install = "go install golang.org/x/tools/gopls@latest".into();
    let mut zig = spec("zig", "zls", &[], &["zig", "zon"], &["build.zig"]);
    zig.install = "install zls (https://zigtools.org/zls)".into();
    let mut dart =
        spec("dart", "dart", &["language-server", "--protocol=lsp"], &["dart"], &["pubspec.yaml"]);
    dart.install = "install the Dart or Flutter SDK and put its bin folder on PATH".into();
    let mut lua = spec("lua", "lua-language-server", &[], &["lua"], &[".luarc.json"]);
    lua.install = "install lua-language-server".into();
    vec![rust, go, ts, python, pylsp, c, zig, dart, lua]
}

/// The built-in servers with the config's changes applied.
pub fn merged(extra: &[ServerSpec]) -> Vec<ServerSpec> {
    let mut all = builtin();
    for s in extra {
        all.retain(|b| b.name != s.name);
        all.push(s.clone());
    }
    all
}

struct Live {
    client: tokio::sync::OnceCell<Result<Arc<Client>, String>>,
    used: Mutex<Instant>,
}

pub struct Code {
    pub settings: Settings,
    servers: Vec<ServerSpec>,
    live: Mutex<HashMap<(String, PathBuf), Arc<Live>>>,
    reaper: AtomicBool,
    scans: Mutex<HashMap<PathBuf, Scan>>,
    /// Why a server last would not start, by server and folder, until it does.
    failures: Mutex<HashMap<(String, PathBuf), String>>,
}

/// When a project was last scanned, and what was found.
type Scan = (Instant, Arc<Vec<Detected>>);

/// How far down a project to look for the files that show which languages it uses.
const SCAN_DEPTH: usize = 4;
const SCAN_ENTRIES: usize = 20_000;
const SCAN_AGE: Duration = Duration::from_secs(30);

/// A language found in a project: which server (by position in the list) and the folder to run it in.
#[derive(Clone, Debug, PartialEq)]
pub struct Detected {
    pub server: usize,
    pub dir: PathBuf,
}

/// Where a file's server runs, and whether the project showed it uses that language.
#[derive(Clone, Debug)]
struct Picked {
    spec: ServerSpec,
    dir: PathBuf,
    found: bool,
}

impl Default for Code {
    fn default() -> Self {
        Code::new(Settings::default())
    }
}

impl Code {
    pub fn new(settings: Settings) -> Code {
        let servers = merged(&settings.lsp);
        Code {
            settings,
            servers,
            live: Mutex::default(),
            reaper: AtomicBool::new(false),
            scans: Mutex::default(),
            failures: Mutex::default(),
        }
    }

    /// The role of the message that carries diagnostics.
    pub fn role(&self) -> &str {
        match self.settings.role.trim() {
            "" => "user",
            r => r,
        }
    }

    fn serves_any(&self, path: &Path) -> bool {
        self.servers.iter().any(|s| s.serves(path))
    }

    /// Find the languages a project uses, from the files that give them away (`Cargo.toml`,
    /// `go.mod`, `package.json`...) anywhere in its first few levels. Each language is given the
    /// outermost folder it was found in, which is where its server should run: a Rust workspace is
    /// one server, not one for each crate. A project can hold several, say Rust at the top and a
    /// web app under `web/`.
    pub fn detect(&self, root: &Path) -> Arc<Vec<Detected>> {
        if let Some((at, found)) = lock(&self.scans).get(root)
            && at.elapsed() < SCAN_AGE
        {
            return found.clone();
        }
        let mut hits: Vec<Detected> = Vec::new();
        let walk = WalkBuilder::new(root)
            .require_git(false)
            .follow_links(false)
            .max_depth(Some(SCAN_DEPTH))
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|e| !SKIPPED.iter().any(|s| e.file_name() == *s))
            .build();
        // a folder like the home directory has no end; stop looking after a while
        for e in walk.flatten().take(SCAN_ENTRIES) {
            if !e.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy();
            let Some(dir) = e.path().parent() else { continue };
            for (i, s) in self.servers.iter().enumerate() {
                if !s.command.trim().is_empty() && s.markers.iter().any(|m| *m == name) {
                    hits.push(Detected { server: i, dir: dir.to_path_buf() });
                }
            }
        }
        // a server with no markers serves every project, from the top
        for (i, s) in self.servers.iter().enumerate() {
            if !s.command.trim().is_empty() && s.markers.is_empty() {
                hits.push(Detected { server: i, dir: root.to_path_buf() });
            }
        }
        // keep the outermost folder of each language
        hits.sort_by_key(|d| (d.server, d.dir.components().count()));
        let mut kept: Vec<Detected> = Vec::new();
        for h in hits {
            if !kept.iter().any(|k| k.server == h.server && h.dir.starts_with(&k.dir)) {
                kept.push(h);
            }
        }
        let kept = Arc::new(kept);
        lock(&self.scans).insert(root.to_path_buf(), (Instant::now(), kept.clone()));
        kept
    }

    /// The server for a file, and the folder it runs in. Where several servers speak the same
    /// language (pyright and pylsp) the first one that is installed wins.
    fn pick(&self, file: &Path, root: &Path) -> Option<Picked> {
        let detected = self.detect(root);
        let mut best: Option<(bool, Picked)> = None;
        for (i, spec) in self.servers.iter().enumerate() {
            if !spec.serves(file) {
                continue;
            }
            let at = detected.iter().find(|d| d.server == i && file.starts_with(&d.dir));
            let picked = Picked {
                spec: spec.clone(),
                dir: at.map_or_else(|| root.to_path_buf(), |d| d.dir.clone()),
                found: at.is_some(),
            };
            let ok = picked.found && spec.installed(&picked.dir);
            // a language the project uses beats one it does not, an installed server beats a missing one
            let better = match &best {
                None => true,
                Some((bok, b)) => (ok, picked.found) > (*bok, b.found),
            };
            if better {
                best = Some((ok, picked));
            }
        }
        best.map(|(_, p)| p)
    }

    /// The servers to ask about a project as a whole: one for each language it uses.
    fn for_project(&self, root: &Path) -> Vec<(ServerSpec, PathBuf)> {
        let detected = self.detect(root);
        let mut out: Vec<(ServerSpec, PathBuf)> = Vec::new();
        for d in detected.iter() {
            let spec = &self.servers[d.server];
            // two servers for one language in one folder: keep the first that is installed
            let rival = detected.iter().any(|o| {
                let other = &self.servers[o.server];
                o.server != d.server
                    && o.dir == d.dir
                    && other.overlaps(spec)
                    && ((other.installed(&o.dir) && !spec.installed(&d.dir))
                        || (other.installed(&o.dir) == spec.installed(&d.dir)
                            && o.server < d.server))
            });
            if !rival {
                out.push((spec.clone(), d.dir.clone()));
            }
        }
        out
    }

    /// The language servers of a project and how far each has got, for the chat's status line.
    /// Starts nothing: a server nothing has needed yet is `Idle`.
    pub fn status(&self, root: &Path) -> Vec<Lsp> {
        let Ok(root) = std::fs::canonicalize(root) else { return Vec::new() };
        let mut out = Vec::new();
        for (spec, dir) in self.for_project(&root) {
            let key = (spec.name.clone(), dir.clone());
            let live = lock(&self.live).get(&key).cloned();
            let failure = lock(&self.failures).get(&key).cloned();
            let (health, detail) = if !spec.installed(&dir) {
                (LspHealth::Missing, spec.install.clone())
            } else if let Some(live) = live {
                match live.client.get() {
                    None => (LspHealth::Starting, String::new()),
                    Some(Ok(c)) => (c.health(), String::new()),
                    Some(Err(e)) => (LspHealth::Failed, e.clone()),
                }
            } else if let Some(e) = failure {
                (LspHealth::Failed, e)
            } else {
                (LspHealth::Idle, String::new())
            };
            out.push(Lsp { name: spec.name.clone(), dir: show(&dir, &root), health, detail });
        }
        out
    }

    /// What the project uses and whether the server for it can run, for `agentbench-server code`.
    pub fn report(&self, root: &Path) -> Vec<(ServerSpec, PathBuf, bool)> {
        self.for_project(root)
            .into_iter()
            .map(|(s, d)| {
                let ok = s.installed(&d);
                (s, d, ok)
            })
            .collect()
    }

    fn evict(&self) {
        let mut live = lock(&self.live);
        live.retain(|_, l| lock(&l.used).elapsed() < IDLE);
        while live.len() > MAX_LIVE {
            let oldest = live.iter().min_by_key(|(_, l)| *lock(&l.used)).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => live.remove(&k),
                None => break,
            };
        }
    }

    fn forget(&self, key: &(String, PathBuf)) {
        lock(&self.live).remove(key);
    }

    async fn client(
        self: &Arc<Self>,
        spec: &ServerSpec,
        root: &Path,
    ) -> Result<Arc<Client>, String> {
        let program = spec.program(root).ok_or_else(|| {
            let how = if spec.install.is_empty() {
                String::new()
            } else {
                format!(" ({})", spec.install)
            };
            format!(
                "{} is not installed (no `{}` on PATH or in the usual tool folders); install it{how}, or point `command` at it under [[code.lsp]] in agentbench.toml",
                spec.name, spec.command
            )
        })?;
        if !self.reaper.swap(true, Ordering::SeqCst) {
            let weak: Weak<Code> = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    match weak.upgrade() {
                        Some(code) => code.evict(),
                        None => break,
                    }
                }
            });
        }
        let key = (spec.name.clone(), root.to_path_buf());
        for _ in 0..2 {
            self.evict();
            let live = lock(&self.live)
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(Live { client: Default::default(), used: Mutex::new(Instant::now()) })
                })
                .clone();
            *lock(&live.used) = Instant::now();
            let started = live
                .client
                .get_or_init(|| async {
                    Client::start(
                        &spec.name,
                        &program,
                        &spec.args,
                        root,
                        spec.options.clone(),
                        START,
                    )
                    .await
                    .map(Arc::new)
                })
                .await;
            match started {
                Ok(c) if c.alive() => {
                    lock(&self.failures).remove(&key);
                    return Ok(c.clone());
                }
                // it died since: start it again
                Ok(_) => self.forget(&key),
                Err(e) => {
                    let e = format!("could not start {}: {e}", spec.name);
                    self.forget(&key);
                    lock(&self.failures).insert(key, e.clone());
                    return Err(e);
                }
            }
        }
        Err(format!("{} keeps stopping", spec.name))
    }

    pub async fn run(
        self: &Arc<Self>,
        tool: &str,
        args: &Value,
        ws: &Workspace,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let root = std::fs::canonicalize(ws.root()).map_err(|e| e.to_string())?;
        match tool {
            "code_outline" => self.outline(args, ws, &root, stop).await,
            "code_find" => self.find(args, ws, &root, stop).await,
            "code_diagnostics" => self.diagnostics(args, ws, &root, stop).await,
            other => Err(format!("{other} is not a code tool")),
        }
    }

    /// The server for the file `path`, started and told about the file's language.
    async fn for_path(
        self: &Arc<Self>,
        path: &Path,
        root: &Path,
    ) -> Result<(Arc<Client>, String), String> {
        let p = self.pick(path, root).ok_or_else(|| self.no_server(path))?;
        let client = self.client(&p.spec, &p.dir).await?;
        Ok((client, p.spec.language_of(path)))
    }

    fn no_server(&self, path: &Path) -> String {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("(none)");
        let known: Vec<String> = self
            .servers
            .iter()
            .filter(|s| !s.command.trim().is_empty())
            .map(|s| format!("{} ({})", s.name, s.extensions.join(", ")))
            .collect();
        format!(
            "no language server is set up for .{ext} files; the ones that are: {}. Add one with [[code.lsp]] in agentbench.toml. Use search_files and read_file meanwhile",
            known.join("; ")
        )
    }

    // ---- code_outline ------------------------------------------------------------------

    async fn outline(
        self: &Arc<Self>,
        args: &Value,
        ws: &Workspace,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let given = files::text(args, &files::PATH).unwrap_or("");
        let at = canonical(&ws.resolve_for(given, Access::Read)?)?;
        let depth = files::number(args, &["depth"]);
        let mut lines: Vec<String> = Vec::new();
        let mut skipped = Vec::new();
        if at.is_file() {
            let (client, lang) = self.for_path(&at, root).await?;
            let syms = client.symbols(&at, &lang, stop).await?;
            lines.push(format!("{}  ({} lines)", show(&at, root), count_lines(&at)));
            tree(&syms, depth.unwrap_or(6), 1, &mut lines);
        } else {
            let mut found: Vec<PathBuf> = Vec::new();
            let walk = WalkBuilder::new(&at)
                .hidden(false)
                .require_git(false)
                .follow_links(false)
                .max_depth(Some(12))
                .sort_by_file_name(|a, b| a.cmp(b))
                .filter_entry(|e| !SKIPPED.iter().any(|s| e.file_name() == *s))
                .build();
            for e in walk.flatten() {
                if e.file_type().is_some_and(|t| t.is_file()) && self.serves_any(e.path()) {
                    found.push(e.path().to_path_buf());
                }
            }
            if found.is_empty() {
                return Err(format!(
                    "{} holds no source files a language server is set up for",
                    show(&at, root)
                ));
            }
            let total = found.len();
            found.truncate(OUTLINE_FILES);
            for f in &found {
                if lines.len() >= OUTLINE_LINES {
                    skipped.push(show(f, root));
                    continue;
                }
                let (client, lang) = match self.for_path(f, root).await {
                    Ok(c) => c,
                    Err(e) => return Err(e),
                };
                let syms = client.symbols(f, &lang, stop).await?;
                lines.push(show(f, root));
                tree(&syms, depth.unwrap_or(1), 1, &mut lines);
            }
            if total > found.len() {
                skipped.push(format!("{} more files", total - found.len()));
            }
        }
        let mut out = cap_lines(lines, OUTLINE_LINES, "outline a smaller folder or one file");
        if !skipped.is_empty() {
            out.push_str(&format!("\n[not shown: {}]", skipped.join(", ")));
        }
        Ok(out)
    }

    // ---- code_find -----------------------------------------------------------------------

    async fn find(
        self: &Arc<Self>,
        args: &Value,
        ws: &Workspace,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let asked = files::text(args, &["name", "query", "symbol", "pattern"])
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or("missing `name`")?
            .to_string();
        let (qualifier, name) = split_name(&asked);
        if name.is_empty() {
            return Err(format!("{asked:?} has no name in it"));
        }
        let kinds = match files::text(args, &["kind", "type"]) {
            Some(k) if !k.trim().is_empty() => Some(
                kinds_for(k).ok_or_else(|| {
                    format!(
                        "{k:?} is not a kind; use function, method, struct, enum, trait, class, interface, module, const, type, variant or field"
                    )
                })?,
            ),
            _ => None,
        };
        let want_body = flag(args, &["body"]).unwrap_or(true);
        let want_refs = flag(args, &["refs", "references", "used_by"]).unwrap_or(true);
        let deps = flag(args, &["deps", "dependencies"]).unwrap_or(false);
        let max_refs = files::number(args, &["max_refs"]).unwrap_or(REFS).clamp(1, 500);
        let max_lines = files::number(args, &["max_lines"]).unwrap_or(BODY_LINES).clamp(10, 2000);
        let limit = files::number(args, &["limit"]).unwrap_or(MATCHES).clamp(1, 30);
        let given = files::text(args, &files::PATH).unwrap_or("");
        let scope = canonical(&ws.resolve_for(given, Access::Read)?)?;

        // which servers to ask
        let specs: Vec<(ServerSpec, PathBuf)> = if scope.is_file() {
            let p = self.pick(&scope, root).ok_or_else(|| self.no_server(&scope))?;
            vec![(p.spec, p.dir)]
        } else {
            self.for_project(root)
                .into_iter()
                // inside a folder, only the languages that are used there
                .filter(|(_, dir)| scope.starts_with(dir) || dir.starts_with(&scope))
                .collect()
        };
        if specs.is_empty() {
            return Err(format!(
                "no language was recognised in this project; servers are chosen by files like {}. Add one with [[code.lsp]] in agentbench.toml. Use search_files meanwhile",
                self.servers
                    .iter()
                    .filter_map(|s| s.markers.first().cloned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let mut exact: Vec<(Arc<Client>, ServerSpec, Found)> = Vec::new();
        let mut near: Vec<Found> = Vec::new();
        let mut waiting = false;
        let mut errors: Vec<String> = Vec::new();
        for (spec, dir) in &specs {
            let client = match self.client(spec, dir).await {
                Ok(c) => c,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            if !client.ready(INDEXING, stop).await {
                waiting = true;
            }
            let found = if scope.is_file() {
                in_file(&client, spec, &scope, &name, stop).await?
            } else {
                client.find(&name, stop).await?
            };
            for f in found {
                if !deps && !is_local(&f.path, root) {
                    continue;
                }
                if !scope.is_file() && scope != root && !f.path.starts_with(&scope) {
                    continue;
                }
                if !spec.serves(&f.path) {
                    continue;
                }
                if f.name == name {
                    exact.push((client.clone(), spec.clone(), f));
                } else {
                    near.push(f);
                }
            }
        }
        if exact.is_empty() && !errors.is_empty() && near.is_empty() {
            return Err(errors.join("; "));
        }

        // narrow the exact hits by the qualifier and the kind, which need the file's units
        let mut hits: Vec<Hit> = Vec::new();
        for (client, spec, found) in exact {
            let lang = spec.language_of(&found.path);
            let syms = client.symbols(&found.path, &lang, stop).await?;
            let Some(i) = locate(&syms, &found) else { continue };
            if let Some(k) = &kinds
                && !k.contains(&syms[i].kind)
            {
                continue;
            }
            if !qualifier.is_empty() && !qualifies(&qualifier, &syms, i, &found, root) {
                continue;
            }
            hits.push(Hit { client, lang, syms, i, path: found.path });
        }
        hits.sort_by(|a, b| (&a.path, a.sym().range.start).cmp(&(&b.path, b.sym().range.start)));
        hits.dedup_by(|a, b| a.path == b.path && a.sym().sel == b.sym().sel);

        let mut out: Vec<String> = Vec::new();
        if hits.is_empty() {
            out.push(format!("no unit named {asked:?} was found"));
            let mut similar: Vec<String> = Vec::new();
            for c in near.iter().filter(|c| kinds.as_ref().is_none_or(|k| k.contains(&c.kind))) {
                let line = format!(
                    "  {} {}  {}:{}",
                    kind_word(c.kind),
                    qualified(c),
                    show(&c.path, root),
                    c.at.map_or(0, |p| p.line + 1)
                );
                if !similar.contains(&line) {
                    similar.push(line);
                }
            }
            if !similar.is_empty() {
                let more = similar.len().saturating_sub(15);
                similar.truncate(15);
                out.push("names that contain it:".into());
                out.extend(similar);
                if more > 0 {
                    out.push(format!("  [{more} more]"));
                }
            }
            if waiting {
                out.push(indexing_note());
            }
            out.extend(errors);
            return Ok(out.join("\n"));
        }

        let total = hits.len();
        hits.truncate(limit);
        for hit in &hits {
            out.push(hit.describe(root));
            if want_body {
                out.extend(body(hit, root, max_lines));
            }
            if want_refs {
                match used_by(hit, root, deps, max_refs, stop).await {
                    Ok(lines) => out.extend(lines),
                    Err(e) => out.push(format!("used by: could not be found ({e})")),
                }
            }
            out.push(String::new());
        }
        if total > hits.len() {
            out.push(format!(
                "[{} more units are named {name}; name the type or module in front (Type::{name}), or pass path or kind]",
                total - hits.len()
            ));
        }
        if waiting {
            out.push(indexing_note());
        }
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        Ok(out.join("\n"))
    }

    // ---- code_diagnostics ------------------------------------------------------------------

    async fn diagnostics(
        self: &Arc<Self>,
        args: &Value,
        ws: &Workspace,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let mut paths: Vec<String> = Vec::new();
        if let Some(list) = args.get("paths").and_then(Value::as_array) {
            paths.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(p) = files::text(args, &files::PATH) {
            paths.push(p.to_string());
        }
        if paths.is_empty() {
            return Err("missing `path`: the file to check".into());
        }
        let floor = match files::text(args, &["severity", "min_severity"]).unwrap_or("warning") {
            "error" | "errors" => 1,
            "warning" | "warnings" => 2,
            "info" | "information" => 3,
            _ => 4,
        };
        let mut out = Vec::new();
        for p in paths {
            let file = canonical(&ws.resolve_for(&p, Access::Read)?)?;
            if !file.is_file() {
                return Err(format!("{} is not a file", show(&file, root)));
            }
            let (client, lang) = self.for_path(&file, root).await?;
            let ready = client.ready(INDEXING, stop).await;
            let all = client.diagnostics(&file, &lang, stop).await?;
            let mut list: Vec<Diag> = all.into_iter().filter(|d| d.severity <= floor).collect();
            list.sort_by_key(|d| (d.pos, d.severity));
            if list.is_empty() {
                out.push(format!("{}: no problems", show(&file, root)));
            } else {
                out.push(format!("{}: {}", show(&file, root), summary(&list)));
                out.extend(diag_lines(&list, &show(&file, root), DIAGS_PER_FILE));
            }
            if !ready {
                out.push(indexing_note());
            }
        }
        Ok(out.join("\n"))
    }

    /// After the agent changed these files: what the language servers make of them.
    pub async fn after_edit(
        self: &Arc<Self>,
        paths: &[PathBuf],
        root: &Path,
        stop: &CancellationToken,
    ) -> Vec<Report> {
        let mut out = Vec::new();
        if !self.settings.diagnostics {
            return out;
        }
        let Ok(root) = std::fs::canonicalize(root) else { return out };
        let mut seen = HashSet::new();
        for p in paths {
            let Ok(file) = std::fs::canonicalize(p) else { continue };
            if !file.starts_with(&root) || !seen.insert(file.clone()) {
                continue;
            }
            let Some(Picked { spec, dir, found }) = self.pick(&file, &root) else { continue };
            if !found || !spec.installed(&dir) {
                continue;
            }
            let Ok(client) = self.client(&spec, &dir).await else { continue };
            if !client.ready(INDEXING_AFTER_EDIT, stop).await {
                continue;
            }
            let lang = spec.language_of(&file);
            let Ok(all) = client.diagnostics(&file, &lang, stop).await else { continue };
            let mut errors: Vec<Diag> = all.into_iter().filter(|d| d.severity == 1).collect();
            errors.sort_by_key(|d| d.pos);
            out.push(Report { path: file, server: spec.name.clone(), errors });
        }
        out
    }
}

/// What a server found wrong in a file after an edit.
pub struct Report {
    pub path: PathBuf,
    pub server: String,
    pub errors: Vec<Diag>,
}

/// The note to put in front of the model, or None when there is nothing to say. `bad` holds the
/// files last reported as having errors, so that a file that is now fine is said to be fine.
pub fn render(reports: &[Report], root: &Path, bad: &mut HashSet<PathBuf>) -> Option<String> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut lines = Vec::new();
    for r in reports {
        let name = show(&r.path, &root);
        if r.errors.is_empty() {
            if bad.remove(&r.path) {
                lines.push(format!("{name}: no errors now ({})", r.server));
            }
            continue;
        }
        bad.insert(r.path.clone());
        lines.push(format!("{name}: {} ({})", summary(&r.errors), r.server));
        lines.extend(diag_lines(&r.errors, &name, DIAGS_PER_FILE));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "<language-server-diagnostics>\nChecked by the language server after your edit; this is the harness speaking, not the user. Fix what is yours:\n{}\n</language-server-diagnostics>",
        lines.join("\n")
    ))
}

// ---- helpers -----------------------------------------------------------------------------

struct Hit {
    client: Arc<Client>,
    lang: String,
    syms: Arc<Vec<Sym>>,
    i: usize,
    path: PathBuf,
}

impl Hit {
    fn sym(&self) -> &Sym {
        &self.syms[self.i]
    }

    fn describe(&self, root: &Path) -> String {
        let s = self.sym();
        let (from, to) = lines_of(s);
        let owner = chain(&self.syms, self.i);
        let mut head = format!(
            "{} {}  {}:{}{}",
            word_of(s),
            if owner.is_empty() { s.name.clone() } else { format!("{owner}::{}", s.name) },
            show(&self.path, root),
            from + 1,
            if to > from { format!("-{}", to + 1) } else { String::new() },
        );
        if let Some(sig) = signature(&self.path, s) {
            head.push_str(&format!("\n  {sig}"));
        }
        head
    }
}

fn indexing_note() -> String {
    "[the language server was still indexing the project, so this may be incomplete; ask again in a moment]".into()
}

fn flag(args: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| args.get(*k)?.as_bool())
}

fn canonical(p: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn show(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() { ".".into() } else { text }
}

fn is_local(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rel| !rel.components().any(|c| SKIPPED.iter().any(|s| c.as_os_str() == *s)))
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path).map(|t| t.lines().count()).unwrap_or(0)
}

/// `a::b::c` and `a.b.c` and `a#c` split into what is in front and the name.
pub fn split_name(asked: &str) -> (Vec<String>, String) {
    let cleaned = asked.replace("::", ".").replace(['#', '/'], ".");
    let mut parts: Vec<String> =
        cleaned.split('.').map(|p| p.trim().trim_end_matches("()").to_string()).collect();
    parts.retain(|p| !p.is_empty());
    let name = parts.pop().unwrap_or_default();
    (parts, name)
}

/// The word for a unit: rust-analyzer files `impl` blocks as objects, which says nothing.
fn word_of(s: &Sym) -> &'static str {
    if s.kind == 19 && s.name.starts_with("impl") { "impl" } else { kind_word(s.kind) }
}

pub fn kind_word(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "mod",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "trait",
        12 => "fn",
        13 => "var",
        14 => "const",
        15 => "string",
        16 => "number",
        17 => "bool",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "variant",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type-param",
        _ => "item",
    }
}

/// The symbol kinds a word like "function" or "trait" stands for.
pub fn kinds_for(word: &str) -> Option<Vec<u32>> {
    Some(match word.trim().to_lowercase().as_str() {
        "function" | "fn" | "func" | "def" => vec![12, 6, 9],
        "method" => vec![6, 9, 12],
        "struct" => vec![23],
        "enum" => vec![10],
        "trait" | "interface" | "protocol" => vec![11],
        "class" => vec![5],
        "type" | "alias" | "typedef" => vec![26, 5, 23, 10, 11],
        "module" | "mod" | "namespace" | "package" => vec![2, 3, 4],
        "const" | "constant" | "static" => vec![14],
        "variable" | "var" | "let" => vec![13, 14],
        "variant" | "member" => vec![22],
        "field" | "property" => vec![8, 7],
        "unit" | "item" | "any" => (1..=26).collect(),
        _ => return None,
    })
}

/// Cut a long list of lines, saying how many went.
fn cap_lines(mut lines: Vec<String>, cap: usize, hint: &str) -> String {
    if lines.len() > cap {
        let more = lines.len() - cap;
        lines.truncate(cap);
        lines.push(format!("[{more} more lines; {hint}]"));
    }
    lines.join("\n")
}

/// The first and last line of a unit, from 0. A range that ends at the start of a line ends on
/// the line before.
fn lines_of(s: &Sym) -> (u32, u32) {
    let (a, b) = (s.range.start.line, s.range.end.line);
    if s.range.end.col == 0 && b > a { (a, b - 1) } else { (a, b) }
}

fn source_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The line the name is on, which is the signature for most languages.
fn signature(path: &Path, s: &Sym) -> Option<String> {
    let src = source_lines(path);
    let line = src.get(s.sel.line as usize)?.trim();
    let line = line.trim_end_matches('{').trim_end();
    Some(clip(line, 220))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

/// Reduce an impl block's name to the type it is for: `impl<T> Show for Wrapper<T>` is `Wrapper`.
fn clean(name: &str) -> String {
    let mut n = name.trim();
    if let Some(rest) = n.strip_prefix("impl") {
        n = rest.trim_start();
        if n.starts_with('<') {
            let mut depth = 0;
            let end = n
                .char_indices()
                .find(|(_, c)| {
                    match c {
                        '<' => depth += 1,
                        '>' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map_or(0, |(i, _)| i + 1);
            n = n[end..].trim_start();
        }
        if let Some((_, target)) = n.split_once(" for ") {
            n = target;
        }
    }
    n.split('<').next().unwrap_or(n).trim().to_string()
}

/// The names a unit sits inside, outermost first, as `a::b`.
fn chain(syms: &[Sym], i: usize) -> String {
    let mut names = Vec::new();
    let mut at = syms[i].parent;
    while let Some(p) = at {
        names.push(clean(&syms[p].name));
        at = syms[p].parent;
    }
    names.reverse();
    names.join("::")
}

fn qualified(f: &Found) -> String {
    if f.container.is_empty() { f.name.clone() } else { format!("{}::{}", f.container, f.name) }
}

/// The unit in `syms` that a search hit points at.
fn locate(syms: &[Sym], f: &Found) -> Option<usize> {
    let at = f.at?;
    syms.iter()
        .position(|s| s.name == f.name && s.sel == at)
        .or_else(|| syms.iter().position(|s| s.name == f.name && s.range.contains(at)))
}

/// Whether the names given in front of a unit are the ones around it: its parents, its file's
/// module, or the container the server named.
fn qualifies(want: &[String], syms: &[Sym], i: usize, f: &Found, root: &Path) -> bool {
    let mut around: Vec<String> = Vec::new();
    let mut at = syms[i].parent;
    while let Some(p) = at {
        around.push(clean(&syms[p].name));
        at = syms[p].parent;
    }
    around.extend(f.container.split("::").map(clean));
    if let Ok(rel) = f.path.strip_prefix(root) {
        for c in rel.components() {
            let c = c.as_os_str().to_string_lossy();
            around.push(c.split('.').next().unwrap_or(&c).to_string());
        }
    }
    want.iter().all(|w| around.iter().any(|a| a == w))
}

/// The units named `name` in one file, found by reading the file's own outline.
async fn in_file(
    client: &Arc<Client>,
    spec: &ServerSpec,
    file: &Path,
    name: &str,
    stop: &CancellationToken,
) -> Result<Vec<Found>, String> {
    let syms = client.symbols(file, &spec.language_of(file), stop).await?;
    let lower = name.to_lowercase();
    Ok(syms
        .iter()
        .enumerate()
        .filter(|(_, s)| s.name.to_lowercase().contains(&lower))
        .map(|(i, s)| Found {
            name: s.name.clone(),
            kind: s.kind,
            container: chain(&syms, i),
            path: file.to_path_buf(),
            at: Some(s.sel),
        })
        .collect())
}

fn tree(syms: &[Sym], depth: usize, indent: usize, out: &mut Vec<String>) {
    let limit = depth.max(1);
    for s in syms {
        if s.depth >= limit {
            continue;
        }
        let (from, to) = lines_of(s);
        let lines =
            if to > from { format!("{}-{}", from + 1, to + 1) } else { format!("{}", from + 1) };
        let detail =
            if s.detail.is_empty() { String::new() } else { format!("  {}", clip(&s.detail, 100)) };
        out.push(format!(
            "{}{} {}{}  [{}]",
            "  ".repeat(indent + s.depth),
            word_of(s),
            s.name,
            detail,
            lines
        ));
    }
}

/// The source of a unit, or when it is long and has parts, its parts.
fn body(hit: &Hit, root: &Path, max: usize) -> Vec<String> {
    let s = hit.sym();
    let (from, to) = lines_of(s);
    let src = source_lines(&hit.path);
    if src.is_empty() {
        return Vec::new();
    }
    let to = (to as usize).min(src.len() - 1);
    let len = to + 1 - (from as usize).min(to);
    let parts = inside(&hit.syms, hit.i);
    if len > max && !parts.is_empty() {
        let mut out = vec![format!(
            "  {len} lines; its parts (code_find one by name, or read_file {} with an offset):",
            show(&hit.path, root)
        )];
        let mut lines = Vec::new();
        let sub: Vec<Sym> = hit.syms[parts].to_vec();
        let base = sub.iter().map(|x| x.depth).min().unwrap_or(0);
        let sub: Vec<Sym> = sub
            .into_iter()
            .map(|mut x| {
                x.depth -= base;
                x
            })
            .collect();
        tree(&sub, 2, 1, &mut lines);
        out.extend(lines.into_iter().take(max));
        return out;
    }
    let mut out = vec!["  ```".to_string()];
    let shown = len.min(max);
    for (n, line) in src[from as usize..from as usize + shown].iter().enumerate() {
        out.push(format!("  {:>5}  {}", from as usize + n + 1, line));
    }
    if len > shown {
        out.push(format!(
            "  [{} more lines; read_file {} offset={}]",
            len - shown,
            show(&hit.path, root),
            from as usize + shown + 1
        ));
    }
    out.push("  ```".to_string());
    out
}

/// Who uses a unit: the places the server finds, each with the unit it sits in.
async fn used_by(
    hit: &Hit,
    root: &Path,
    deps: bool,
    max: usize,
    stop: &CancellationToken,
) -> Result<Vec<String>, String> {
    let s = hit.sym();
    let refs = hit.client.references(&hit.path, &hit.lang, s.sel, stop).await?;
    let (from, to) = lines_of(s);
    let mut places: Vec<(PathBuf, Pos)> = refs
        .into_iter()
        .filter(|(p, _)| deps || is_local(p, root))
        // a unit using itself is not news
        .filter(|(p, pos)| !(p == &hit.path && pos.line >= from && pos.line <= to))
        .collect();
    places.sort();
    places.dedup();
    if places.is_empty() {
        return Ok(vec!["used by: nothing found".into()]);
    }
    let total = places.len();
    let mut out = vec![format!("used by {total}:")];
    // one line per enclosing unit, with the lines it uses it on
    let mut by_unit: BTreeMap<(PathBuf, u32, String), Vec<u32>> = BTreeMap::new();
    let mut cache: HashMap<PathBuf, Arc<Vec<Sym>>> = HashMap::new();
    for (path, pos) in &places {
        if !cache.contains_key(path) {
            let lang = hit.lang.clone();
            let syms = match hit.client.symbols(path, &lang, stop).await {
                Ok(s) => s,
                Err(_) => Arc::new(Vec::new()),
            };
            cache.insert(path.clone(), syms);
        }
        let syms = &cache[path];
        let (start, label) = match enclosing(syms, *pos) {
            Some(j) => {
                let owner = chain(syms, j);
                let name = if owner.is_empty() {
                    syms[j].name.clone()
                } else {
                    format!("{owner}::{}", syms[j].name)
                };
                (lines_of(&syms[j]).0, format!("{} {name}", word_of(&syms[j])))
            }
            None => (0, "(top level or imports)".to_string()),
        };
        by_unit.entry((path.clone(), start, label)).or_default().push(pos.line + 1);
    }
    for (shown, ((path, _, label), lines)) in by_unit.iter().enumerate() {
        if shown >= max {
            out.push(format!("  [{} more units use it; raise max_refs]", by_unit.len() - shown));
            break;
        }
        let list: Vec<String> = lines.iter().take(6).map(|l| l.to_string()).collect();
        let more = if lines.len() > 6 { format!(" +{}", lines.len() - 6) } else { String::new() };
        out.push(format!("  {}:{}{}  {label}", show(path, root), list.join(","), more));
    }
    Ok(out)
}

/// The smallest unit that holds a position.
fn enclosing(syms: &[Sym], at: Pos) -> Option<usize> {
    syms.iter()
        .enumerate()
        .filter(|(_, s)| s.range.contains(at))
        .max_by_key(|(_, s)| s.depth)
        .map(|(i, _)| i)
}

fn summary(list: &[Diag]) -> String {
    let n = |sev: u8| list.iter().filter(|d| d.severity == sev).count();
    let mut parts = Vec::new();
    for (sev, word) in [(1, "error"), (2, "warning"), (3, "note"), (4, "hint")] {
        let c = n(sev);
        if c > 0 {
            parts.push(format!(
                "{c} {word}{}",
                if c == 1 || word == "note" && c == 1 { "" } else { "s" }
            ));
        }
    }
    parts.join(", ")
}

fn diag_lines(list: &[Diag], name: &str, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = list
        .iter()
        .take(cap)
        .map(|d| {
            let sev = match d.severity {
                1 => "error",
                2 => "warning",
                3 => "note",
                _ => "hint",
            };
            let code = if d.code.is_empty() { String::new() } else { format!("[{}]", d.code) };
            let msg = d.message.lines().next().unwrap_or_default();
            format!("  {name}:{}:{}  {sev}{code} {}", d.pos.line + 1, d.pos.col + 1, clip(msg, 300))
        })
        .collect();
    if list.len() > cap {
        out.push(format!("  [{} more]", list.len() - cap));
    }
    out
}

/// The specs of the code tools, for the model.
pub fn specs() -> Vec<egui_bench::agent::tools::Spec> {
    use egui_bench::agent::tools::{Spec, integer, object, string};
    vec![
        Spec::new(
            "code_outline",
            "Break a source file, or a folder of them, into its units: modules, structs, enums, traits, functions, \
             constants, with the lines each covers. Use it to see what is in code without reading it. A file is \
             broken down fully; a folder shows each file's top-level units, and `depth` goes further. Works through \
             a language server, so it needs one for the language (Rust, Go, TypeScript, Python and C are set up if \
             installed; the answer says when one is missing).",
            object(
                json!({
                    "path": string("A source file or a folder, default the whole worktree"),
                    "depth": integer("How many levels of nesting to show; a file defaults to all of them, a folder to 1"),
                }),
                &[],
            ),
        ),
        Spec::new(
            "code_find",
            "Find a function, struct, enum, trait, class, constant or other unit by its exact name, and get its \
             signature, where it is, its source and every place that uses it (the unit each use sits in), without \
             searching or reading raw files. Qualify a name to pick one of several: `Config::load`, `search::search`. \
             Works through the project's language server, so it follows the language's own rules for what a use is. \
             Prefer it to search_files when you know what you are looking for. If no unit has the name, it lists \
             names that contain it.",
            object(
                json!({
                    "name": string("The unit's name, or Type::name / module::name to pick one"),
                    "kind": string("Only this kind: function, method, struct, enum, trait, class, interface, module, const, type, variant, field"),
                    "path": string("Look only in this file or folder"),
                    "body": { "type": "boolean", "description": "Include the source, default true" },
                    "refs": { "type": "boolean", "description": "Include what uses it, default true" },
                    "max_refs": integer("Most using units to list, default 40"),
                    "max_lines": integer("Most source lines to show, default 120; a longer unit shows its parts"),
                    "limit": integer("Most matching units to show, default 8"),
                    "deps": { "type": "boolean", "description": "Also look in dependencies, default false" },
                }),
                &["name"],
            ),
        ),
        Spec::new(
            "code_diagnostics",
            "Ask the language server what is wrong in a file as it is on disk now: errors, warnings and the rest, \
             with line, column and code. Faster than building to see a type error. It checks one file, so a change \
             that breaks another file shows up there, not here.",
            object(
                json!({
                    "path": string("The file to check"),
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Several files to check" },
                    "severity": {
                        "type": "string",
                        "enum": ["error", "warning", "info", "hint"],
                        "description": "The least severe to list, default warning",
                    },
                }),
                &[],
            ),
        ),
    ]
}

pub const TOOLS: [&str; 3] = ["code_outline", "code_find", "code_diagnostics"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn names_split_on_the_separators_of_many_languages() {
        let s = |a: &str| split_name(a);
        assert_eq!(s("search"), (vec![], "search".to_string()));
        assert_eq!(s("Config::load"), (vec!["Config".to_string()], "load".to_string()));
        assert_eq!(s("a::b::c()"), (vec!["a".to_string(), "b".to_string()], "c".to_string()));
        assert_eq!(
            s("pkg.Type.Method"),
            (vec!["pkg".to_string(), "Type".to_string()], "Method".to_string())
        );
        assert_eq!(s("Class#method"), (vec!["Class".to_string()], "method".to_string()));
    }

    #[test]
    fn impl_blocks_are_named_for_their_type() {
        assert_eq!(clean("impl Native"), "Native");
        assert_eq!(clean("impl<T> Show for Wrapper<T>"), "Wrapper");
        assert_eq!(clean("impl Agent for Native"), "Native");
        assert_eq!(clean("Config"), "Config");
        assert_eq!(clean("Vec<T>"), "Vec");
    }

    #[test]
    fn kind_words_cover_the_common_names() {
        assert!(kinds_for("function").unwrap().contains(&12));
        assert!(kinds_for("Struct").unwrap().contains(&23));
        assert!(kinds_for("interface").unwrap().contains(&11));
        assert!(kinds_for("nonsense").is_none());
        assert_eq!(kind_word(10), "enum");
    }

    #[test]
    fn built_in_servers_can_be_replaced_or_switched_off() {
        let mut mine = spec("rust", "my-ra", &[], &["rs"], &[]);
        mine.options = None;
        let all = merged(&[
            mine,
            spec("zig", "zls", &[], &["zig"], &["build.zig"]),
            spec("go", "", &[], &["go"], &[]),
        ]);
        let find = |n: &str| all.iter().find(|s| s.name == n).unwrap();
        assert_eq!(find("rust").command, "my-ra");
        assert_eq!(all.iter().filter(|s| s.name == "rust").count(), 1);
        assert!(find("zig").serves(Path::new("a/b.zig")));
        assert!(!find("go").serves(Path::new("main.go")), "an empty command switches a server off");
    }

    #[test]
    fn files_get_the_language_id_of_their_extension() {
        let code = Code::default();
        let id = |f: &str| {
            let p = Path::new(f);
            code.servers.iter().find(|s| s.serves(p)).map(|s| s.language_of(p))
        };
        assert_eq!(id("x.tsx").as_deref(), Some("typescriptreact"));
        assert_eq!(id("x.ts").as_deref(), Some("typescript"));
        assert_eq!(id("x.h").as_deref(), Some("c"));
        assert_eq!(id("x.cpp").as_deref(), Some("cpp"));
        assert_eq!(id("x.rs").as_deref(), Some("rust"));
        assert!(id("x.unknown").is_none());
    }

    #[test]
    fn the_config_reads_from_toml() {
        let c: crate::Config = toml::from_str(
            "[code]\nrole = \"developer\"\ndiagnostics = false\n\n[[code.lsp]]\nname = \"zig\"\ncommand = \"zls\"\nextensions = [\"zig\"]\nmarkers = [\"build.zig\"]\n[code.lsp.ids]\nzon = \"zon\"\n",
        )
        .unwrap();
        assert_eq!(c.code.role, "developer");
        assert!(!c.code.diagnostics);
        assert_eq!(c.code.lsp[0].extensions, ["zig"]);
        assert_eq!(c.code.lsp[0].ids["zon"], "zon");
        let d: crate::Config = toml::from_str("").unwrap();
        assert!(d.code.diagnostics && d.code.role == "user" && d.code.lsp.is_empty());
    }

    fn project(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "").unwrap();
        }
        dir
    }

    /// The languages found, as (server name, folder relative to the project).
    fn found(code: &Code, root: &Path) -> Vec<(String, String)> {
        let mut out: Vec<_> = code
            .detect(root)
            .iter()
            .map(|d| (code.servers[d.server].name.clone(), show(&d.dir, root)))
            .collect();
        out.sort();
        out
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn a_project_is_recognised_by_the_files_that_give_its_language_away() {
        let dir = project(&["Cargo.toml", "src/lib.rs"]);
        assert_eq!(found(&Code::default(), dir.path()), [pair("rust", ".")]);
        let dir = project(&["go.mod", "main.go"]);
        assert_eq!(found(&Code::default(), dir.path()), [pair("go", ".")]);
        let dir = project(&["README.md"]);
        assert!(found(&Code::default(), dir.path()).is_empty());
    }

    #[test]
    fn several_languages_in_one_project_each_get_their_own_folder() {
        let dir = project(&[
            "Cargo.toml",
            "crates/a/Cargo.toml",
            "crates/b/Cargo.toml",
            "web/package.json",
            "web/tsconfig.json",
            "web/node_modules/dep/package.json",
            "target/debug/Cargo.toml",
            "tools/gen/pyproject.toml",
        ]);
        // one Rust server for the workspace, not one per crate; nothing from target or node_modules
        assert_eq!(
            found(&Code::default(), dir.path()),
            [
                pair("python", "tools/gen"),
                pair("python-pylsp", "tools/gen"),
                pair("rust", "."),
                pair("typescript", "web")
            ]
        );
    }

    #[test]
    fn only_the_outermost_folder_of_a_language_is_kept() {
        let dir = project(&[
            "app/package.json",
            "app/packages/ui/package.json",
            "app/packages/ui/tsconfig.json",
        ]);
        assert_eq!(found(&Code::default(), dir.path()), [pair("typescript", "app")]);
    }

    #[test]
    fn a_file_is_served_from_the_folder_its_language_was_found_in() {
        let dir = project(&["Cargo.toml", "web/package.json", "web/src/a.ts", "src/lib.rs"]);
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let code = Code::default();
        let ts = code.pick(&root.join("web/src/a.ts"), &root).unwrap();
        assert_eq!(
            (ts.spec.name.as_str(), show(&ts.dir, &root), ts.found),
            ("typescript", "web".to_string(), true)
        );
        let rs = code.pick(&root.join("src/lib.rs"), &root).unwrap();
        assert_eq!((rs.spec.name.as_str(), show(&rs.dir, &root)), ("rust", ".".to_string()));
        // a language the project shows no sign of is still reachable, but is not "found"
        let go = code.pick(&root.join("x.go"), &root).unwrap();
        assert!(!go.found);
        assert!(code.pick(&root.join("x.unknown"), &root).is_none());
    }

    #[test]
    fn of_two_servers_for_one_language_the_installed_one_is_used() {
        let dir = project(&["pyproject.toml", "a.py"]);
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("fake-pylsp"), "").unwrap();
        let servers = vec![
            spec("python", "no-such-pyright-xyz", &[], &["py"], &["pyproject.toml"]),
            spec(
                "python-pylsp",
                bin.join("fake-pylsp").to_str().unwrap(),
                &[],
                &["py"],
                &["pyproject.toml"],
            ),
        ];
        let code = Code::new(Settings { lsp: vec![], ..Settings::default() });
        let code = Code { servers, ..code };
        assert_eq!(code.pick(&root.join("a.py"), &root).unwrap().spec.name, "python-pylsp");
        let names: Vec<_> = code.for_project(&root).iter().map(|(s, _)| s.name.clone()).collect();
        assert_eq!(names, ["python-pylsp"]);
        let report = code.report(&root);
        assert_eq!(report.len(), 1);
        assert!(report[0].2);
    }

    #[test]
    fn the_status_tells_what_is_missing_without_starting_anything() {
        let dir = project(&["Cargo.toml", "web/package.json"]);
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("ra"), "").unwrap();
        let servers =
            vec![spec("rust", bin.join("ra").to_str().unwrap(), &[], &["rs"], &["Cargo.toml"]), {
                let mut s =
                    spec("typescript", "no-such-tsserver-xyz", &[], &["ts"], &["package.json"]);
                s.install = "npm install -g typescript-language-server".into();
                s
            }];
        let code = Code { servers, ..Code::default() };
        let status = code.status(&root);
        let got: Vec<_> =
            status.iter().map(|l| (l.name.as_str(), l.dir.as_str(), l.health)).collect();
        assert_eq!(
            got,
            [("rust", ".", LspHealth::Idle), ("typescript", "web", LspHealth::Missing)]
        );
        assert_eq!(status[1].detail, "npm install -g typescript-language-server");
        assert!(lock(&code.live).is_empty(), "looking does not start a server");
        assert!(code.status(Path::new("/no/such/folder")).is_empty());
    }

    #[test]
    fn a_server_is_found_in_the_project_before_the_path() {
        let dir = project(&["node_modules/.bin/typescript-language-server"]);
        let ts = builtin().into_iter().find(|s| s.name == "typescript").unwrap();
        assert_eq!(
            ts.program(dir.path()).unwrap(),
            dir.path().join("node_modules/.bin/typescript-language-server")
        );
        let missing = spec("x", "no-such-program-xyz", &[], &["x"], &[]);
        assert!(missing.program(dir.path()).is_none() && !missing.installed(dir.path()));
        assert!(spec("off", "", &[], &["x"], &[]).program(dir.path()).is_none());
    }

    fn sym(name: &str, kind: u32, depth: usize, parent: Option<usize>, from: u32, to: u32) -> Sym {
        use super::super::lsp::Span;
        Sym {
            name: name.into(),
            kind,
            detail: String::new(),
            range: Span { start: Pos { line: from, col: 0 }, end: Pos { line: to, col: 1 } },
            sel: Pos { line: from, col: 4 },
            parent,
            depth,
        }
    }

    #[test]
    fn the_unit_around_a_position_is_the_deepest_one() {
        let syms = vec![
            sym("impl Native", 5, 0, None, 10, 40),
            sym("tool", 6, 1, Some(0), 12, 30),
            sym("other", 12, 0, None, 50, 60),
        ];
        assert_eq!(enclosing(&syms, Pos { line: 20, col: 3 }), Some(1));
        assert_eq!(enclosing(&syms, Pos { line: 35, col: 0 }), Some(0));
        assert_eq!(enclosing(&syms, Pos { line: 45, col: 0 }), None);
        assert_eq!(chain(&syms, 1), "Native");
        assert_eq!(chain(&syms, 2), "");
    }

    #[test]
    fn a_qualifier_matches_parents_modules_and_containers() {
        let syms = vec![sym("impl Config", 5, 0, None, 1, 9), sym("load", 6, 1, Some(0), 2, 8)];
        let f = Found {
            name: "load".into(),
            kind: 6,
            container: String::new(),
            path: PathBuf::from("/p/src/lib.rs"),
            at: Some(Pos { line: 2, col: 4 }),
        };
        let root = Path::new("/p");
        let q = |w: &[&str]| {
            qualifies(&w.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &syms, 1, &f, root)
        };
        assert!(q(&["Config"]));
        assert!(q(&["lib"]));
        assert!(q(&["src", "Config"]));
        assert!(!q(&["Other"]));
        assert_eq!(locate(&syms, &f), Some(1));
    }

    #[test]
    fn diagnostics_are_worded_for_the_model() {
        let d = |sev, line, msg: &str| Diag {
            pos: Pos { line, col: 4 },
            severity: sev,
            code: if sev == 1 { "E0425".into() } else { String::new() },
            source: String::new(),
            message: msg.into(),
        };
        let list = vec![d(1, 11, "cannot find value `x`\nmore"), d(2, 20, "unused")];
        assert_eq!(summary(&list), "1 error, 1 warning");
        let lines = diag_lines(&list, "src/a.rs", 20);
        assert_eq!(lines[0], "  src/a.rs:12:5  error[E0425] cannot find value `x`");
        assert_eq!(lines[1], "  src/a.rs:21:5  warning unused");
        assert_eq!(diag_lines(&list, "a", 1).last().unwrap(), "  [1 more]");
    }

    #[test]
    fn a_file_that_is_fixed_is_said_to_be_fixed_once() {
        let root = tempfile::tempdir().unwrap();
        let file = std::fs::canonicalize(root.path()).unwrap().join("a.rs");
        let err = Diag {
            pos: Pos { line: 0, col: 0 },
            severity: 1,
            code: String::new(),
            source: String::new(),
            message: "bad".into(),
        };
        let mut bad = HashSet::new();
        let broken = [Report { path: file.clone(), server: "rust".into(), errors: vec![err] }];
        let note = render(&broken, root.path(), &mut bad).unwrap();
        assert!(
            note.contains("a.rs: 1 error (rust)") && note.contains("a.rs:1:1  error bad"),
            "{note}"
        );
        let fixed = [Report { path: file.clone(), server: "rust".into(), errors: vec![] }];
        let note = render(&fixed, root.path(), &mut bad).unwrap();
        assert!(note.contains("a.rs: no errors now"), "{note}");
        assert!(
            render(&fixed, root.path(), &mut bad).is_none(),
            "nothing to say about a file that was never bad"
        );
    }

    /// Needs rust-analyzer; skipped where it is not installed.
    #[tokio::test(flavor = "multi_thread")]
    async fn it_finds_units_and_their_users_through_rust_analyzer() {
        if super::super::resolve(Path::new("rust-analyzer")).is_none() {
            eprintln!("rust-analyzer is not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &str, c: &str| {
            let f = dir.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        };
        w("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        w(
            "src/lib.rs",
            "pub mod shape;\n\npub fn total(items: &[shape::Shape]) -> f64 {\n    items.iter().map(|s| s.area()).sum()\n}\n",
        );
        w(
            "src/shape.rs",
            "pub enum Kind {\n    Round,\n    Square,\n}\n\npub struct Shape {\n    pub kind: Kind,\n    pub size: f64,\n}\n\nimpl Shape {\n    pub fn area(&self) -> f64 {\n        match self.kind {\n            Kind::Round => 3.14 * self.size * self.size,\n            Kind::Square => self.size * self.size,\n        }\n    }\n}\n",
        );
        w(
            "src/main.rs",
            "fn main() {\n    let s = demo::shape::Shape { kind: demo::shape::Kind::Round, size: 2.0 };\n    println!(\"{}\", demo::total(&[s]));\n}\n",
        );
        let ws = Workspace::open(dir.path()).unwrap();
        let code = Arc::new(Code::default());
        let stop = CancellationToken::new();
        let run = |tool: &'static str, args: Value| {
            let (code, ws, stop) = (code.clone(), ws.clone(), stop.clone());
            async move { code.run(tool, &args, &ws, &stop).await }
        };

        assert_eq!(code.status(dir.path())[0].health, LspHealth::Idle);
        let out = run("code_outline", json!({"path": "src/shape.rs"})).await.unwrap();
        // an outline needs one file, not the whole index, so the server may still be reading
        assert!(matches!(
            code.status(dir.path())[0].health,
            LspHealth::Indexing | LspHealth::Ready
        ));
        assert!(
            out.contains("enum Kind")
                && out.contains("variant Round")
                && out.contains("struct Shape"),
            "{out}"
        );
        assert!(out.contains("method area") && out.contains("impl Shape"), "{out}");

        let out = run("code_find", json!({"name": "Shape::area"})).await.unwrap();
        assert_eq!(
            code.status(dir.path())[0].health,
            LspHealth::Ready,
            "a find waits for the index"
        );
        assert!(out.starts_with("method Shape::area  src/shape.rs:12-17"), "{out}");
        assert!(out.contains("match self.kind"), "{out}");
        assert!(out.contains("src/lib.rs:4") && out.contains("fn total"), "{out}");

        let out =
            run("code_find", json!({"name": "Kind", "kind": "enum", "body": false})).await.unwrap();
        assert!(out.contains("enum Kind  src/shape.rs:1-4"), "{out}");
        assert!(
            out.contains("used by 4")
                && out.contains("field Shape::kind")
                && out.contains("method Shape::area")
                && out.contains("fn main"),
            "{out}"
        );

        let out = run("code_find", json!({"name": "Shap"})).await.unwrap();
        assert!(out.contains("no unit named") && out.contains("struct Shape"), "{out}");

        // an edit that breaks a file is reported, and fixing it is too
        let path = dir.path().join("src/lib.rs");
        fs::write(&path, "pub mod shape;\n\npub fn total() -> f64 {\n    let n: u32 = \"text\";\n    missing_name\n}\n").unwrap();
        let reports = code.after_edit(std::slice::from_ref(&path), dir.path(), &stop).await;
        let mut bad = HashSet::new();
        let note = render(&reports, dir.path(), &mut bad).expect("an error is reported");
        assert!(note.contains("src/lib.rs:4:18") && note.contains("expected u32"), "{note}");
        let out = run("code_diagnostics", json!({"path": "src/lib.rs"})).await.unwrap();
        assert!(out.contains("error"), "{out}");
        fs::write(&path, "pub mod shape;\n\npub fn total() -> f64 {\n    1.0\n}\n").unwrap();
        let reports = code.after_edit(&[path], dir.path(), &stop).await;
        let note = render(&reports, dir.path(), &mut bad).unwrap();
        assert!(note.contains("no errors now"), "{note}");
    }
}
