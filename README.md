<img src="assets/icon.svg" width="64" alt="">

# agentbench

A small server that runs coding agents in your git projects and keeps an issue
tracker beside them, and a bench-style window onto it for desktop, web and
Android.

A **project** is a git repository's main checkout. Every **worktree** off it is
a branch an agent can work in, and every **session** is a chat with one agent
pinned to one worktree. Click a worktree to start a chat in it. Worktrees an agent makes on its own are
picked up within 15 seconds. **Issues** belong to a project; starting an agent
on one gives it a fresh worktree named after the issue and moves the issue to
doing.

The **projects** tab lists every project with its automation level, running
agents, issue counts, vision and last activity, filtered and sorted. Opening one
shows its overview, vision and goals with issue counts, agents at work, issues,
memory notes and recent activity, stacked so it reads on a phone.

Everything lives in one SQLite file. The window talks to the server only over
its REST API, so it can sit on another machine.

## Install

Download a `.deb`, `.msi`, `.dmg` or archive for your platform from the
[releases](https://github.com/v0l/agentbench/releases). Each release lists its
changes in [CHANGELOG.md](CHANGELOG.md).

## Run

```sh
cargo run --release -p agentbench
```

A build from source runs Whisper and read aloud on an NVIDIA card by default, so it needs the
CUDA toolkit. A machine without a card still runs the build, on the CPU. On a Mac use
`--no-default-features --features metal`, and anywhere without the toolkit
`--no-default-features --features local-speech`.

The window starts the server in the background if it is not already running,
and connects to it. The server keeps running after the window closes so agents
carry on, and a later window reuses it. To use a server on another machine
instead, pick remote under **settings**.

The **settings** key holds everything: which server, the window's font, code
font and text size (any font installed on the machine, kept on this device),
and the native agent's model, levels and chat titles (kept on the server).

The same binary is the server: `agentbench serve`, `agentbench add ~/git/snort`,
`agentbench token`, `native agents`. `agentbench-server` is the same commands
without the window, for headless machines.

The server listens on `127.0.0.1:7420`; set `listen` in `agentbench.toml` to
change it. Reaching it from elsewhere (DNAT, a tunnel, a reverse proxy) is up
to you. Every `/api` route needs `Authorization: Bearer <token>`;
`agentbench token --rotate` makes a new one. Its log is `server.log` beside the
database.

Data lives in `$AGENTBENCH_HOME` (default `~/.local/share/agentbench`):
`agentbench.db` and an optional `agentbench.toml`.

## Chatting in a terminal

```sh
agentbench chat                      # a standalone chat in this folder
agentbench chat summarise ~/notes.md # first message given on the command line
agentbench chat --bare               # no memory notes, skills or MCP servers
agentbench chat --save               # keep a plain transcript under the data folder
agentbench chat --server             # a tracked chat on the server
agentbench chat -c                   # carry on with the latest tracked chat
agentbench chat --server --project snort
```

The default is a standalone chat for one-off questions. The agent runs inside the `chat`
process in the current folder, with the same model, prices and language servers as the
server (read from `agentbench.toml` and the saved model settings, read-only). It starts no
server, needs no token, writes nothing to the database and shows nowhere else: the
conversation lives in memory and goes when you leave, and long chats still fold and compact.
The tracker and supervisor tools (issues, sessions, `remember`, `recall`, autopilot) are not
offered. Memory notes, the user instruction files and skills are read-only context; leave them
out with `--no-memory`, `--no-skills` and `--no-mcp`, or all three with `--bare`. `--save`
writes `chat-<time>.md` under `chats/` in the data folder when the chat ends.

`--server` (also `--tracked`, `-c`, `--project` and `--agent`) uses the server instead: it starts
it if it is not running, or use `--url` and `--token` (or `AGENTBENCH_URL` and
`AGENTBENCH_TOKEN`) for one on another machine. It is the same chat as in the window, so the
two show each other's messages. Enter sends, alt+enter or `\` then enter
adds a line, up recalls earlier messages, esc stops the agent, ctrl+o shows tool output
and thinking, and `/help` lists the rest. Leaving (ctrl+c twice, ctrl+d or `/quit`) does not
stop the agent.

## Watching several chats

The five keys beside **settings** tile the chat area: one chat, two side by side, two
stacked, a 2x2 grid or a 3x2 grid. Click in a tile to focus it (it gets the bright border), then click
a chat in the list and it replaces what that tile shows; the other tiles stay put. Clicking
a chat that is already on screen just focuses its tile. The focused tile is the one that
gets dropped or pasted files and that **new chat** fills. Each tile has its own draft.
Narrow windows show one chat.

## GitHub issues and pull requests

A project can take its work from its GitHub repo instead of the built-in tracker: issues people open there are triaged by the autopilot, ready ones are worked and turned into pull requests, and pull requests are reviewed. Nothing is posted or merged until you approve it in the project's outbox. See [docs/github-flow.md](docs/github-flow.md) for the trust rules, the post modes, the webhook and how to turn it off.

## Taking control of a supervised session

A session the supervisor or the autopilot started reports to its parent. To work in it yourself, right click it and choose **take control**: it is released, shows as user controlled, and the supervisor and autopilot stop messaging, judging, nudging or merging it. Chatting in it does not release it by itself. **hand back to supervisor** (or the supervisor's `watch_session` with `adopt`) puts it under the supervisor again. The same is `POST /api/sessions/{id}/release` and `/adopt`.

## Agents

`agentbench` is the built in agent: it talks to any OpenAI compatible server, works
in the session's worktree with file tools and bash, searches the web
(`web_search`) and reads, queries (`prompt`, answered by the title model) or downloads pages
(`web_fetch`), loads skills from
`~/.agents/skills`, looks for new ones in the skills.sh directory (`find_skills`, which
never installs anything itself), and has tools for the tracker (`list_issues`, `get_issue`,
`file_issue`, `update_issue`, `comment_on_issue`) and
`ask_user`, which puts a question in the chat and waits for the answer. Options
show one to a row, `multiple` lets the user tick several, and `questions` asks a
whole set at once, answered together. A
message sent while it works steers it at the next step.

Beyond that it can:

- run things in the background: `bash` with `background` answers with a handle,
  `bash_output` reads what it printed since the last look and `bash_kill` stops it;
- hand work to other agents: `start_session` (an issue or a prompt, in its own
  worktree), `wait_for_session`, `send_message`, and `list_sessions`,
  `read_session` and `session_status` to watch them. `start_session` can name
  another `project` (`list_projects`), but the chat first asks you: *Allow once*,
  *Always allow in that project*, *Always allow, any project* or *Deny*. The
  agent cannot grant this itself. `GET`/`DELETE /api/settings/cross-project`
  shows and clears the standing permissions;
- manage worktrees with `list_worktrees` and `create_worktree`. `close_session`
  stops a session, deletes its chat and, when it was the last one in its
  worktree, removes a clean worktree and deletes its branch if merged. A dirty
  tree (unless `force`) or an unmerged branch is kept and the result says why.
  It never removes the main worktree or the caller's own. For a worktree with no
  session left, pass `worktree` (id) or `branch` instead of `id`. Closing a chat
  in the app follows the same rule;
- ask for a new project with `add_project`: import a local checkout, create an
  empty repo, or clone an https or ssh remote (default `~/git/<name>`). Paths must
  be under the home folder. It never adds anything itself: it files a request the
  user confirms on a card (mode, path, name, remote, branch and HEAD, free disk
  for a clone) in the supervisor chat, and the asking session is told the new
  project id or the refusal;
- keep a checklist for the user to follow with `todo_write`;
- remember things about a project with `remember` and `recall`; the notes open
  every later session of that project;
- look at its own UI with `screenshot`, which needs Chrome, Chromium or Edge;
- tell the user with `notify_user`, a notice in the chat and a desktop
  notification where `notify-send` (Linux) or `osascript` (macOS) exists.

On first launch the window opens a setup: sign in with a Claude Code or Codex subscription (it runs `claude auth login` / `codex login` in a pane and never stores the login itself), or add an Anthropic key, an OpenAI key, an OpenAI compatible endpoint or a local model, then pick the low, medium, high and supervisor models. Keys are kept in the server's database. Run it again from settings, under *set up providers*.

Set its model in `agentbench.toml`:

```toml
[model]
url = "http://localhost:8001/v1"
model = "qwen3.8-27b"
key = "..."
```

Any other CLI that prints JSON lines can be added without code:

```toml
listen = "127.0.0.1:7420"

[[agent]]
name = "aider"
program = "aider"
args = ["--json", "--yes"]
resume_args = ["--session", "{session}"]
model_args = ["--model", "{model}"]
session = "/session_id"
text = "/message/content"
error = "/error"
input_tokens = "/usage/input"
output_tokens = "/usage/output"
```

The pointers are JSON pointers into each line. `{prompt}` in `args` passes the
prompt as an argument instead of stdin.

### MCP servers

The native agent can use MCP servers, local (stdio) or remote (streamable HTTP). It reads them
from `~/.agents/mcp.json` and then from `.mcp.json` in the worktree, where a server of the same
name replaces the global one, in the format other clients use:

```json
{
  "mcpServers": {
    "kicad": { "command": "node", "args": ["~/git/KiCAD-MCP-Server/dist/index.js"] },
    "docs": { "url": "https://bun.com/docs/mcp" },
    "ha": { "url": "https://ha.example/api/mcp", "headers": { "Authorization": "Bearer ${HA_TOKEN}" } }
  }
}
```

Their tools sit behind one `mcp` tool, so a dozen servers do not add hundreds of tools to every
request: with no arguments it lists the servers, with `server` it starts that one and lists its
tools, and with `server`, `tool` and `args` it calls one. A server starts the first time a chat
uses it, in that chat's worktree, and stops after 30 idle minutes. Pictures a tool sends back are
shown to the model. `${VAR}` is filled in from the server's environment, and `timeout` sets a
call's limit in seconds (300 by default). The old SSE transport and OAuth sign-in are not
supported. MCP servers run as you, with no sandbox, so add only ones you trust.

### Code tools

The native agent can read code by its units instead of by raw text, through whatever
language servers are installed:

- `code_outline` breaks a file (or the top level of a folder) into modules, structs,
  enums, traits and functions, with the lines each covers.
- `code_find` takes an exact name (`Config::load`, `Shape::area`, optionally `kind`)
  and answers with the signature, location, source, and **every unit that uses it**.
  A name that matches nothing lists the names that contain it.
- `code_diagnostics` lists what the server reports for a file.

After the agent writes or edits a file, the server's *errors* in it are added to the
conversation as a message from the harness, without the agent asking (and once more
when they are fixed). `role = "developer"` sends it with the `developer` role that
OpenAI's API has for this; the default is `user`, which every server accepts.

Nothing is specific to a language, and nothing needs configuring. When the agent first
uses a code tool, the project is scanned (four levels down, skipping `target`,
`node_modules` and the like) for the files that give a language away, and each
language gets a server run from the outermost folder it was found in. A Rust
workspace is one server, not one per crate; a repo with a Go backend and a
`frontend/` gets one of each, each in its own folder.

| Language | Server | Found by |
| --- | --- | --- |
| Rust | `rust-analyzer` | `Cargo.toml` |
| Go | `gopls` | `go.mod`, `go.work` |
| TypeScript, JavaScript | `typescript-language-server` | `tsconfig.json`, `package.json`, `jsconfig.json` |
| Python | `pyright-langserver`, else `pylsp` | `pyproject.toml`, `setup.py`, `requirements.txt` |
| C, C++ | `clangd` | `compile_commands.json`, `CMakeLists.txt`, `compile_flags.txt` |
| Zig | `zls` | `build.zig` |
| Dart, Flutter | `dart language-server` | `pubspec.yaml` |
| Lua | `lua-language-server` | `.luarc.json` |

A server is looked for in the project's `node_modules/.bin`, on `PATH`, then in
`~/.cargo/bin`, `~/go/bin`, `~/.local/bin`, nvm's node folders and the like, since
a service started by systemd has a short `PATH`. A language whose server is missing
is reported with the command to install it, to the agent, on the chat's status line
(`LSP rust typescript indexing go missing`, hover for the detail), and in:

```
$ agentbench-server code ~/git/hub
go             .                            missing: go install golang.org/x/tools/gopls@latest
typescript     frontend                     missing: npm install -g typescript-language-server typescript
```

Add or replace any server in `agentbench.toml`:

```toml
[code]
diagnostics = true   # show errors after an edit
role = "user"        # or "developer"

[[code.lsp]]
name = "ruby"
command = "solargraph"
args = ["stdio"]
extensions = ["rb"]         # files it serves
markers = ["Gemfile"]       # files that show a project uses it; none means every project
install = "gem install solargraph"
# language = "ruby"         # the language id sent to it, default the name
# ids = { erb = "erb" }     # language id by extension
# options = { }             # initializationOptions
```

A server starts the first time it is needed, one per project folder, and stops after
twenty idle minutes. An empty `command` turns a built-in one off.

## Attachments

Drop files on the chat, paste a picture with ctrl+v, or use **attach**. Up to 8
files of 20 MB each go with a message and are kept with the chat. The agentbench
agent is shown pictures, sound and video directly; CLI agents added in the
config are told where each file was written so they can read it.

## Prompt templates

A Markdown file in a prompts folder becomes a `/name` command in the app composer,
the terminal chat, the REST send endpoint and the supervisor's `start_session` and
`send_message` prompts. The text is expanded before the agent sees it, and the chat
shows a notice with the template name. An unknown `/name` is sent as written.

Folders, later ones override earlier ones with the same name: `~/.pi/agent/prompts`,
`~/.agents/prompts`, then `<repo>/.agents/prompts`. Files are re-read on use.

```markdown
---
description: Review staged git changes
argument-hint: "[focus]"
---
Review the staged changes. Focus on ${1:-correctness, security and error handling}.
```

`/review concurrency` runs it with `concurrency`. The syntax is pi's: `$1`, `$@`,
`$ARGUMENTS`, `${1:-default}`, `${@:-default}`, `${@:N}`, `${@:N:L}`. Arguments use
shell-like quotes (`"two words"`), an apostrophe inside a word is plain text, and
`\"` inside double quotes is a quote. `GET /api/projects/{id}/prompts` lists them.

## Chat titles

A new chat is named from the start of your first message. Once the agent has
answered, a small model reads the conversation and writes a proper title in its
place. It only ever replaces a title cut from your message; a title you gave,
or an issue's, is left alone. If the model is down the stand-in stays and the
next turn tries again. `/title` asks for a fresh one, and `/title my words`
sets it yourself.

The model is any OpenAI compatible server, so a small one beside the big one
works well (ollama, llama.cpp, vllm). Set it in the window: under **settings**, the
**chat titles** section below the model has its own address, key and model. Leave a field empty and it uses the
native agent's, so with nothing set the agent's own model names the chats.
Settings are saved on the server like the agent's model, and the key is never
read back or sent anywhere but the address beside it.

`agentbench.toml` can supply a default for a server that has not been set from the
window; what you save in the window wins.

```toml
[title_model]
url = "http://localhost:11434/v1"
model = "qwen2.5:1.5b"
```

## Quota and cost

The native agent reads `usage.cost_usd` and `usage.quota` (`window`, `used_pct`,
`resets_at`) from each reply, or the `x-yalr-cost-usd` and `x-yalr-quota-*`
headers, and polls `GET {server}/quota` every three minutes. The header bar of
the app and the status line of the TUI show it as `5h 62%, resets 14:30 UTC`,
with spend as `$1.20 today, $0.80 project`. `GET /api/usage` returns the same.

- At 75% and again at 90% of a window, every running chat gets one notice:
  `Quota 5h at 78%, resets at 14:30 UTC (in 2h 10m)`. The supervisor's system
  prompt carries the current quota beside the automation levels.
- A rate limit with a known reset (the server's `next_available_at`, else the
  retry-after) does not fail the chat. It waits, shown as `quota exhausted,
  resumes at HH:MM UTC`, and carries on at that time. A wait over an hour also
  raises a desktop notice.
- While any model server is exhausted the autopilot starts no workers, logs
  `paused for quota until HH:MM UTC`, and goes on after the reset.

An issue can say which model it needs: `model_level` is `low`, `med`, `high` or
unset (the default model). The label `model:high` (also `model:low`,
`model:med`) sets it too, which is how GitHub-mirrored issues carry it; the
column wins when both exist. The autopilot starts the worker on the model the
project's level table names (`agentbench issue levels <project> --high <model>`,
or the model levels fields on the project page). An empty entry means low is the
server's low model, med is the default model and high is the server's high
model. A level with no model available, or whose model is out of quota, is never
swapped for a cheaper one: the issue stays in todo with a note and a feed line
(an out-of-quota level is skipped for that tick and the next issue is tried).
Rework and rebase requests go to the same session, so they keep the level.
`start_session {issue}` without a model uses the issue's level, and an issue
tagged high refuses a lower model unless `force` is set. Set it with
`file_issue` and `update_issue` (`model`), `agentbench issue new --model high`,
`agentbench issue set <id> --model med`, or the level control on the issue page
and form. Servers with API 9 or later support it.

Every issue session takes one path, whether the autopilot picked the issue or
the supervisor started it with `start_session {issue}`: the worker finishes,
the supervisor gets one judge request and answers with `judge_session`, and at
ship or above a concluded_pass is merged and pushed, the issue goes to done,
and the session, its worktree and the merged branch are closed. `auto_close`
(on by default, in the autopilot panel and the `autopilot` tool) turns the
closing off. Label an issue `parked` to keep the picker off it. `merge_branch`
is for a branch with no issue.

When a merge does not go through, the comment names the branch, the main
branch, the head and the git error, and the autopilot retries on its own with a
backoff of 30 s, 2 min, 10 min, then every 30 min. A notice goes out at most
once an hour per project.

For a dirty main checkout the supervisor carries the choice out with the
`main_checkout` tool (ship level): `stash`, `commit` (needs `message`, main
branch only), `reset_paths` (`dry_run` first; a tracked modified file needs
`force`) and `ask_session`. All are refused during a merge or rebase.

| Cause | The session is told | Retries |
|---|---|---|
| Conflict found before merging | rebase onto main, the files | after the worker finishes again, 3 follow-ups per head, then `merge-blocked` |
| `git merge` failed, or the branch is not in main afterwards | the git error, run `git merge <main>` and resolve | same |
| Branch missing or moved since the verdict | the reason | same |
| The worker's worktree has uncommitted changes | commit or discard them | same |
| The main checkout has uncommitted changes | nothing; the supervisor gets a report with a reset, commit or stash recommendation (once per 15 minutes), the user only after 30 minutes unhandled | every tick once clean |
| The main checkout is on the wrong branch | nothing, the user is notified | every tick once fixed |
| Push failed after a local merge | nothing, the user is notified | the push is retried, never a second merge |

A worker on a model below the top level (anything but the `high` level) gets a
`check_in` message to the supervisor every `check_in_minutes` of a turn
(default 30), with its time on the issue, commits, dirty files, last bash
commands with exit codes, last `test result:` lines and compactions. The
supervisor answers with the `check_in` tool: `continue`, `message` (send text)
or `escalate`, which stops the session and starts a takeover in the same
worktree on the top model. A check_in is marked stuck, and sent at once and
only once per signal, when the same test or build command failed
`stuck_failed_runs` times in a row (3), a test that passed now fails,
`stuck_compactions` compactions happened in one turn (2), or there was no commit
for `stuck_no_commit_minutes` (45) while dirty files grew. The session list
shows a `stuck` badge while the last check_in was stuck. `check_in_all` also
checks in on top-model sessions; `auto_escalate` (off) escalates stuck sessions
by itself and tells the supervisor. All of these are in the `autopilot` tool.

When a project at autowork or above has no `todo` issue, no running worker and at
least one `backlog` issue, the autopilot sends the supervisor one `groom <project>:`
message with the vision, active goals, the backlog (oldest first, 40 at most) and
the last 10 done issues. The supervisor grooms with `update_issue` and
`comment_on_issue` (promote only issues that serve an active goal, merge
duplicates, close stale ones; with no vision it asks the user for one) and ends
with the `groom_done` tool, whose one-line summary goes to the autopilot feed.
A project is groomed at most once per `groom_hours` (24) and again only after its
backlog changed. `groom` (on) and `groom_hours` are in the `autopilot` tool.

When a server sends no cost, it is worked out from a price table, in USD per
million tokens. Claude models have defaults; a model with no entry, such as a
local one, costs 0. Entries match the start of the model name, the longest wins,
and yours override the defaults:

```toml
[prices."claude-sonnet-4-5"]
input = 3.0
output = 15.0
cache_read = 0.3
cache_write = 3.75
```

## Switching models

A chat can move to a stronger model when it needs one, and back again.

Under **settings**, the **model levels** section sets up `low`, `medium` and `high`.
Each is a model and, if you like, a server and key of its own, so `high` can be
a hosted model while `low` is a local one. A level with no server uses the
native agent's; a level with its own server never sends it the native agent's key. A
level with no model is left out.

- **You:** `/model high` (or the **model** key above the chat) moves the chat to
  that level. `/model some-id` names a model on the native agent's server, and
  `/model` alone goes back to the default. It applies from the next turn.
- **The agent:** once any level is set up, the native agent gets a
  `switch_model` tool with `level` and `reason`. It can go up for a hard bug and
  down for mechanical work, and the switch takes effect on its next step, in the
  middle of a turn. The chat remembers it, and a notice in the transcript says
  what changed and why.

## Web and Android

```sh
cargo build -p agentbench --lib --release --target wasm32-unknown-unknown \
    --no-default-features --features local-speech
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
    target/wasm32-unknown-unknown/release/agentbench.wasm
agentbench-server --web web
```

The server then serves the app at `/`, and the page fills in its own address.

```sh
packaging/android/build.sh                 # debug APK, target/android/agentbench.apk
RELEASE=1 KEYSTORE=... KEYSTORE_PASS=... packaging/android/build.sh   # signed release
ABI=x86_64 packaging/android/build.sh      # emulator
```

That script is the only Android build (needs `cargo-ndk`, the SDK and NDK). `cargo apk` does not ship the Kotlin activity, so paste and notifications break.

## API

| route | |
|---|---|
| `GET /api/overview` | projects, worktrees, issues, sessions, agents and the change cursor |
| `GET /api/changes?since=N&wait=20` | long poll, returns when something changed |
| `POST /api/projects` `{path, name?, create?}` | add a repository; `create` makes the folder and runs `git init` when it is missing or not a repo |
| `POST /api/projects/{id}/worktrees` `{branch}` | make a worktree |
| `DELETE /api/worktrees/{id}?force=true` | remove one, the branch is kept |
| `POST /api/issues` `{project_id, title, body}` | file an issue |
| `PATCH /api/issues/{id}` | change title, body, state, priority, labels |
| `POST /api/issues/{id}/comments` `{body}` | comment |
| `POST /api/issues/{id}/start` `{agent, place, model?, prompt?}` | run an agent on it |
| `POST /api/sessions` `{project_id, agent, place, prompt?}` | start an agent |
| `GET /api/sessions/{id}/messages?after=N` | transcript |
| `POST /api/sessions/{id}/messages` `{text, attachments?}` | next turn; each attachment is `{name, mime, data}`, data base64 |
| `GET /api/sessions/{id}/attachments/{file}` | the bytes of a file sent in that chat |
| `POST /api/sessions/{id}/answer` `{question, answer}` | answer an agent's question |
| `POST /api/sessions/{id}/release`, `/adopt` | take a session from the supervisor, or hand it back |
| `POST /api/sessions/{id}/interrupt` | stop the turn; replies `{interrupted, queued}` with the unread messages it took back |

Sessions are named by UUID, so a deleted chat's id is never handed to a new one.
Projects, worktrees, issues and messages keep plain numbers.

`place` is `{"kind": "main"}`, `{"kind": "existing", "worktree_id": 3}` or
`{"kind": "new", "branch": "fix-login"}`.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
