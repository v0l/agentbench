# foreman

A small server that runs coding agents in your git projects and keeps an issue
tracker beside them, and a bench-style window onto it for desktop, web and
Android.

A **project** is a git repository's main checkout. Every **worktree** off it is
a branch an agent can work in, and every **session** is a chat with one agent
pinned to one worktree. Click a worktree to start a chat in it. Worktrees an agent makes on its own are
picked up within 15 seconds. **Issues** belong to a project; starting an agent
on one gives it a fresh worktree named after the issue and moves the issue to
doing.

Everything lives in one SQLite file. The window talks to the server only over
its REST API, so it can sit on another machine.

## Run

```sh
cargo run --release -p foreman
```

The window starts the server in the background if it is not already running,
and connects to it. The server keeps running after the window closes so agents
carry on, and a later window reuses it. To use a server on another machine
instead, pick remote under **settings**.

The **settings** key holds everything: which server, the window's font, code
font and text size (any font installed on the machine, kept on this device),
and the foreman agent's model, levels and chat titles (kept on the server).

The same binary is the server: `foreman serve`, `foreman add ~/git/snort`,
`foreman token`, `foreman agents`. `foreman-server` is the same commands
without the window, for headless machines.

The server listens on `127.0.0.1:7420`; set `listen` in `foreman.toml` to
change it. Reaching it from elsewhere (DNAT, a tunnel, a reverse proxy) is up
to you. Every `/api` route needs `Authorization: Bearer <token>`;
`foreman token --rotate` makes a new one. Its log is `server.log` beside the
database.

Data lives in `$FOREMAN_HOME` (default `~/.local/share/foreman`):
`foreman.db` and an optional `foreman.toml`.

## Agents

`foreman` is the built in agent: it talks to any OpenAI compatible server, works
in the session's worktree with file tools and bash, loads skills from
`~/.agents/skills`, and has tools for the tracker (`list_issues`, `file_issue`,
`update_issue`, `comment_on_issue`, `list_sessions`, `read_session`) and
`ask_user`, which puts a question in the chat and waits for the answer. Options
show one to a row, `multiple` lets the user tick several, and `questions` asks a
whole set at once, answered together. A
message sent while it works steers it at the next step. Set its model in
`foreman.toml`:

```toml
[model]
url = "http://localhost:8001/v1"
model = "qwen3.8-27b"
key = "..."
```

`pi`, `claude` and `codex` are built in too. Each turn runs the CLI once in the
worktree with the prompt on stdin and reads its JSON event stream; a message
sent while one works runs as the next turn. They are told they run inside
foreman and given the API address and token (`FOREMAN_URL`, `FOREMAN_TOKEN`,
`FOREMAN_SESSION`, `FOREMAN_PROJECT`) so they can use the tracker over REST.
Each keeps its own session files and is resumed by id; foreman keeps the
normalized transcript you read.

Any other CLI that prints JSON lines can be added without code:

```toml
listen = "127.0.0.1:7420"
pi = "/opt/pi/bin/pi"

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
foreman agent's, so with nothing set the agent's own model names the chats.
Settings are saved on the server like the agent's model, and the key is never
read back or sent anywhere but the address beside it.

`foreman.toml` can supply a default for a server that has not been set from the
window; what you save in the window wins.

```toml
[title_model]
url = "http://localhost:11434/v1"
model = "qwen2.5:1.5b"
```

## Switching models

A chat can move to a stronger model when it needs one, and back again.

Under **settings**, the **model levels** section sets up `low`, `medium` and `high`.
Each is a model and, if you like, a server and key of its own, so `high` can be
a hosted model while `low` is a local one. A level with no server uses the
foreman agent's; a level with its own server never sends it the foreman key. A
level with no model is left out.

- **You:** `/model high` (or the **model** key above the chat) moves the chat to
  that level. `/model some-id` names a model on the foreman agent's server, and
  `/model` alone goes back to the default. It applies from the next turn.
- **The agent:** once any level is set up, the foreman agent gets a
  `switch_model` tool with `level` and `reason`. It can go up for a hard bug and
  down for mechanical work, and the switch takes effect on its next step, in the
  middle of a turn. The chat remembers it, and a notice in the transcript says
  what changed and why.

Other agents (pi, claude, codex) have their own model lists, so `/model` with a
model id still works for them, but levels and `switch_model` are the foreman
agent's.

## Web and Android

```sh
cargo build -p foreman --lib --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
    target/wasm32-unknown-unknown/release/foreman.wasm
foreman-server --web web
```

The server then serves the app at `/`, and the page fills in its own address.

```sh
cargo apk build -p foreman --lib --release
```

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
| `POST /api/sessions/{id}/messages` `{text}` | next turn |
| `POST /api/sessions/{id}/answer` `{question, answer}` | answer an agent's question |
| `POST /api/sessions/{id}/interrupt` | stop the turn |

`place` is `{"kind": "main"}`, `{"kind": "existing", "worktree_id": 3}` or
`{"kind": "new", "branch": "fix-login"}`.
