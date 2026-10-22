# foreman

A small server that runs coding agents in your git projects and keeps an issue
tracker beside them, and a bench-style window onto it for desktop, web and
Android.

A **project** is a git repository's main checkout. Every **worktree** off it is
a branch an agent can work in, and every **session** is one agent's
conversation pinned to one worktree. Worktrees an agent makes on its own are
picked up within 15 seconds. **Issues** belong to a project; starting an agent
on one gives it a fresh worktree named after the issue and moves the issue to
doing.

Everything lives in one SQLite file. The window talks to the server only over
its REST API, so it can sit on another machine.

## Run

```sh
cargo run --release -p foreman-server -- add ~/git/snort
cargo run --release -p foreman-server -- token
cargo run --release -p foreman-server
cargo run --release -p foreman
```

The server listens on `127.0.0.1:7420`. Reaching it from elsewhere (DNAT, a
tunnel, a reverse proxy) is up to you. Every `/api` route needs
`Authorization: Bearer <token>`; `foreman-server token --rotate` makes a new
one.

Data lives in `$FOREMAN_HOME` (default `~/.local/share/foreman`):
`foreman.db` and an optional `foreman.toml`.

## Agents

`pi`, `claude` and `codex` are built in. Each turn runs the agent's CLI once in
the session's worktree with the prompt on stdin, and reads its JSON event
stream. The agent keeps its own session files and is resumed by id on the next
turn; foreman keeps the normalized transcript you read.

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
| `POST /api/projects` `{path, name?}` | add a repository |
| `POST /api/projects/{id}/worktrees` `{branch}` | make a worktree |
| `DELETE /api/worktrees/{id}?force=true` | remove one, the branch is kept |
| `POST /api/issues` `{project_id, title, body}` | file an issue |
| `PATCH /api/issues/{id}` | change title, body, state, priority, labels |
| `POST /api/issues/{id}/comments` `{body}` | comment |
| `POST /api/issues/{id}/start` `{agent, place, model?, prompt?}` | run an agent on it |
| `POST /api/sessions` `{project_id, agent, place, prompt?}` | start an agent |
| `GET /api/sessions/{id}/messages?after=N` | transcript |
| `POST /api/sessions/{id}/messages` `{text}` | next turn |
| `POST /api/sessions/{id}/interrupt` | stop the turn |

`place` is `{"kind": "main"}`, `{"kind": "existing", "worktree_id": 3}` or
`{"kind": "new", "branch": "fix-login"}`.
