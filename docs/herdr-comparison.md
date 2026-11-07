# Herdr comparison

Herdr (https://herdr.dev) is an Apache-2.0 runtime for coding agents. This note records what it does,
how that compares with agentbench today, and which ideas we take. It was written from the sources, not
the web pages, at these commits (shallow clones, all Apache-2.0):

| Repo | Commit | What it is |
| --- | --- | --- |
| `herdrdev/herdr-core` | 9ce385d | Rust server and CLI `hc`. Owns terminals and agents. JSON-RPC over a Unix socket or stdio. |
| `herdrdev/herdr-app` | adf94cb | Native GPU app (GPUI) that is one client of core. Config as plain files, JS mods. |
| `herdrdev/herdr` | ee5bfb5 | The older all-in-one TUI multiplexer (its own server, screen detection, integrations). |

Files cited below are paths in those repos.

## What Herdr is, in short

A terminal-first design. Everything is a PTY terminal `tN` that a small holder process keeps alive.
Agents are terminals whose program says what state it is in. Core never guesses from the screen
(`herdr-core/crates/herdr-core/src/status.rs`). The whole thing is scriptable by the agents themselves
(`hc --skill`), so agents start agents, wait for them and report back.

### Solid

- **A real, versioned API.** `schema/hc.openrpc.json` is generated from the Rust types, and a test
  fails when it is stale. `schema/released.openrpc.json` is the last released API, and a test fails on
  any change that breaks a client of it (removed method or field, new required input, narrower input).
  `hc schema` and `rpc.discover` print it. Errors have a stable `kind` and a `hint`.
- **Event stream with a sequence.** `events.subscribe` returns a snapshot with `seq`, then
  notifications; a heartbeat every 15 s; `missed` with a fresh snapshot when a subscriber falls behind.
  Event types: created, lost, restarted, keep, finished, removed, status, turn, title, labels, size,
  foreground. Filters by terminal, `children_of`, labels (`schema/README.md`).
- **Status protocol, OSC 7501.** See the next section.
- **The `turn` counter.** Goes up every time the root record leaves `working`, for any reason
  (`status.rs`, `the_turn_counts_every_way_of_leaving_working`). Clients derive "done" as
  `turn > seen` (`herdr-app/docs/labels.md`), and write `herdr.seen=<turn>` as a terminal label so
  looking at an agent on one client clears "done" on all of them. `herdr.pinned=1` works the same way.
- **Machines.** `hc machine add host --name box`, `hc run --on box`, addresses `box/t3`. The local core
  holds the SSH connection (`ssh box hc connect`, the protocol over stdin/stdout), merges terminal
  lists and events, retries lost connections, and can install or update `hc` on the far side. Custom
  transports with `--via '<command>'` (fly, a Vercel sandbox helper in `docs/via/`). `hc exec` and
  `hc file read|write|edit --on box` give agents hands there with no terminal
  (`herdr-core/docs/machines.md`).
- **Agent hierarchy.** A terminal started inside an hc terminal records its starter as `parent`.
  `hc wait --children all|any`, parent gets typed notes when a child is blocked, errored or gone,
  children die with the parent unless `--outlive-parent` (`src/notes.rs`, `src/wait.rs`). The note is
  only typed in when the parent can take it (`can_take_note`: live, has reported, not blocked).
- **Waits are check-ins.** `hc run` and `hc wait` return after 180 s with `reason: timeout`, exit 124,
  the program still running, and the address to wait on again. `wait --output X` waits for a string
  printed after the wait began. `hc send --wait` returns `not_started` if the agent never began.
- **Guard rails on `send`.** `send` refuses while the target is `blocked` unless `--force`, so an agent
  cannot answer another agent's permission prompt by accident. `interrupt` reports what happened.
  A launch with a task records `first_message: {sent, reason}` and keeps it across restarts.
- **Self-describing skill.** `hc --skill` prints the full agent manual (`src/SKILL.md`, embedded with
  `include_str!`). A tiny `skills/herdr-core/SKILL.md` only tells the agent to run it. `herdr-app --skill`
  does the same for the window. The command reference `docs/commands.md` is generated from `--help`
  and checked in CI (`just docs-check`).
- **Templates.** `~/.config/herdr-core/templates/<name>.md`: header (`command`, `description`, `ready`,
  `submit`, `stop`, `paste_gap_ms`) and a body of opening instructions. `hc run --bg --name review
  reviewer "task"`. Read fresh on each use (`docs/templates.md`).
- **Persistence rules.** `hc run --keep`: a lost program is replaced by a fresh shell in its last
  folder, never by re-running the task; backoff 1.5, 3, 6, 12, 30, 60 s; exit with a code and explicit
  kill are final; server stop parks kept terminals. `finished` reasons: exited, killed, server_stop,
  host_shutdown, holder_lost, stopped_by_restart. Holder processes let core restart and update with
  terminals still running. Layout JSON is saved atomically, corrupt files are moved aside, 3 recovery
  copies and up to 48 timed snapshots are kept (`herdr/docs/.../session-state.mdx`).
- **Config as files.** `~/.config/herdr-app/`: `layouts/<name>.json`, `keys.json`, `settings.json`,
  `mods/<name>/`, `programs.json`, `themes/`, and a generated `AGENTS.md` plus `.d.ts` type files that
  say "this folder is the app's whole interface, edit it". Edits apply live, an invalid file keeps the
  last good one and writes `<name>.error`, `herdr-app check <mod>` validates a mod, `herdr-app shot`
  saves the rendered window as a PNG for the agent to look at, and ctrl-alt-shift-escape resets to the
  default layout (`herdr-app/defaults/AGENTS.md`).
- **Notifications.** Sound (bundled MP3s, overridable) and a clickable toast when an agent becomes
  blocked or finishes while you are not looking; Linux, macOS and Windows native notifications.
- **Security page that says what is true.** No sandbox, socket mode 0700, no API password, trust is
  the account (`herdr-core/docs/security.md`).

### Half built or weak

- Still preview: "protocol generation 1, version 0.x". The `released.openrpc.json` file equals the
  current one, so the compatibility test has never been exercised against a real break.
- Update manifests are checked by SHA-256 from the same HTTPS host, with no separate signing key
  (stated in `security.md`).
- Status for agents that do not speak 7501 is nothing at all in core. The older `herdr` TUI compensates
  with a table of regex rules per agent on the screen text (`herdr/src/detect/manifests/*.toml`) and
  per-agent hook installers (`herdr integration install claude`), which is brittle: the changelog is
  full of "status recognised in more cases" fixes. Its Codex hook was removed in 0.9.5 because it
  produced a warning.
- Notes typed into a parent's terminal rely on the parent being at a prompt. They are a paste into a
  TUI, not a message. This is the cost of a terminal-first design.
- Mods are trusted, unsandboxed JavaScript (`herdr-app/docs/security.md`). Layouts can load mods.
- The SKILL has many platform special cases (macOS bundles, Windows PowerShell, launchd) that show how
  heavy a native-terminal runtime gets.
- OSC 7501 itself is documented only through tests and the parser. There is no standalone spec file in
  the repos.

## OSC 7501, the Program Status Protocol

Exact form, from `herdr-core/crates/ghostty-vt/src/tests.rs` (`program_status_reports_and_title_changes_reach_us`)
and `status.rs` (`Status::encode`):

```text
ESC ] 7501 ; key=value : key=value : ... ST        ST is ESC \ or BEL (0x07)
```

| Key | Meaning |
| --- | --- |
| `state` | `idle`, `working`, `done`, `blocked`, `error`, or `clear` (remove records). Anything else is ignored (`state=bogus` yields no report). |
| `kind` | `permission`, `question` or `auth`. Only with `blocked`. |
| `progress` | 0 to 100. Only with `working` or `blocked`. |
| `id` | Empty for the root record. `a/b` is a child record under `a`. `clear` with an id removes that record and its children. |
| `app` | Program name, for example `claude-code`. |
| `title`, `msg` | Free text, base64 encoded (the test sends `msg=QXBwbHk/` for "Apply?"). |

Rules: a report replaces its record; at most 256 records per terminal; a program asking
`ESC ] 7501 ; ? ST` gets `ESC ] 7501 ; ? ...` back so it can tell it is under a supporting terminal;
the status resets when the program exits; a new viewer's first paint replays the latest report.
`turn` increments each time the root leaves `working`. "Stuck" is `blocked` or `error` and fires a
note to the parent.

States as clients show them (`herdr-app/README.md`): working, blocked (waiting for you), done (finished
while you looked elsewhere), idle. A space's status is the most urgent of its agents:
blocked > done > working > idle.

## What agentbench has today

- One server (`crates/server`), one SQLite file, REST on `127.0.0.1:7420` with a bearer token, a
  long-poll change feed (`/api/changes?since=&wait=`, `api.rs`), a TUI chat, an egui app for desktop,
  web and Android that talks only REST (`crates/app/src/client.rs`).
- Sessions are chats pinned to a worktree. Agents are either the native API agent
  (`agent/native.rs`) or a CLI run turn by turn through a JSON stream decoder (`agent/command.rs`,
  `agent/process.rs`): one process per turn, resumed with a session id. Status is therefore known by
  the runner (`idle`, `running`, `waiting`, `failed`, `stopped`), not derived from output.
- A PTY terminal per session (`terminal.rs`, portable-pty and vt100) behind
  `/api/sessions/{id}/terminal`, used for logins and a shell.
- `parent` column on sessions (`store.rs`), set for sessions the supervisor or autopilot starts.
  Workers are told `PARENT=<id>` and report with `send_message`. Worker questions route to the
  supervisor (#204), system messages wait for supervisor idle (#209), `judge_session` gates merges (#208).
- Supervisor chat, autopilot loop with caps, per-project automation level, triage pass, issue tracker,
  notes with versions, worktrees, review gate (#205 open), onboarding, voice, send_file, a browser tool.
- On restart, `reset_running` marks running or waiting sessions `stopped` and `Runner::resume`
  re-prompts them (`runner.rs`, test `sessions_cut_off_by_a_restart_are_resumed`).
- Config: `agentbench.toml` (listen, few keys), settings in the DB, per-device UI settings.
- Notifications: server `desktop_notice`, Android watch service, "needs you" marker and header count.
- No published API schema, no skill printing endpoint, no scripting CLI beyond `chat`, `add`, `token`.

## Feature table

Effort S under a day, M a few days, L a week or more. Value is for the way we work (a supervisor and
workers across about 25 repos and several machines).

| Herdr feature | agentbench today | Gap | Effort | Value | Decision |
| --- | --- | --- | --- | --- | --- |
| OSC 7501 status per terminal | Status comes from the runner for our own agents. CLI agents run in the shell pane are invisible. | No way for an external or interactive CLI agent to appear with working/blocked/done and count as waiting. | M | High | **Adapt**: parse 7501 in our PTY, plus an HTTP/CLI report so non-terminal agents can report. Issue filed. |
| Screen-text status detection (herdr TUI manifests) | none | Brittle regex tables per agent | L | Low | **Skip**: Herdr itself avoids it in core. We use 7501 or the REST report. |
| Per-agent hook integrations (`herdr integration install`) | none | A Claude Code or Codex hook that calls the report command | S | Medium | **Adapt** as a follow-up inside the status issue (print the hook snippet, do not edit user config). |
| `hc machine add` over SSH, `--on`, `box/t3` addresses | Single server; the app can point at one remote server | Run sessions on the Spark, Mac, VPS and see them in one list | L | High | **Adapt**: stdio transport, federated list. Issue filed. |
| `hc connect` protocol over stdin/stdout | HTTP on a TCP port with a token | No port exposure, SSH does auth | M | High | **Adopt** inside the remote issue. |
| `--via` custom transport | none | Custom commands (fly, containers) | S | Low | **Adopt** as a field of the machine record, same issue. |
| `hc exec` / `hc file` on a machine | Agents' own file tools, local only | Remote tools for an agent running elsewhere | M | Medium | **Adapt**: needed only if the agent runs on a different machine than the files; sessions that run on the machine have local tools. Skip until asked; noted in the remote issue. |
| Remote install/update of `hc` | none | `agentbench machine add --install` | S | Medium | **Adapt** as a step in the remote issue (copy the binary when OS/arch match). |
| Parent/child terminals, `wait --children`, `--outlive-parent` | `parent` column, supervisor tools `watch_session`, `wait_for_session`; no lifetime rule; workers report by message | Children outlive or die by accident; no worker-side wait | M | High | **Adapt**: worker tools and rules. Issue filed. |
| Notes to parent only when it can take them | #209 digest when supervisor idle | Same idea, already done for the supervisor | n/a | n/a | Already have. Extend to any parent in the hierarchy issue. |
| 180 s check-in on waits | `wait_for_session` timeout up to 540 s | Return a structured `timeout` result with the address, never block a turn | S | Medium | **Adopt** in the hierarchy issue. |
| `send` refuses blocked target, `interrupt` reports effect, `first_message.sent` | `send_message` always delivers | An agent can answer another agent's question by accident | S | Medium | **Adopt** in the CLI issue and hierarchy issue. |
| OpenRPC schema generated from types + stale and compat tests | none; routes only in `api.rs` | No documented API; clients are hand-written | M | High | **Adapt**: OpenAPI 3.1 (we are REST, not JSON-RPC). Issue filed. |
| `hc --skill`, `herdr-app --skill`, tiny stub skill that runs it | Agent brief text in `agent/mod.rs` | Outside agents (Claude Code, Codex in a terminal) cannot drive agentbench | S | High | **Adopt**: `agentbench --skill`, `GET /api/skill`. Same issue as the schema. |
| Generated command reference checked in CI | none | Doc drift | S | Low | **Adopt** with the CLI issue. |
| `hc` CLI: ls, get, read, send, wait, events, kill, rm, label | `agentbench chat` only | No scripting from a shell or another agent | M | High | **Adopt** as `abc` over REST. Issue filed. |
| `hc events` with seq, heartbeat, missed, filters | `/api/changes` long poll, no filters, no seq contract | Documented stream, filters, NDJSON | S | Medium | **Adapt** in the CLI issue (add an SSE or NDJSON endpoint, keep the long poll). |
| Labels as shared client state (`herdr.seen`, `herdr.pinned`) | Per-client UI state | "Done" marker does not clear across the phone and the desktop | S | Medium | **Adapt**: a `seen_turn` on the session instead of labels. Issue filed with notifications. |
| `turn` counter | Message ids, no turn count on the session | Needed for seen/done | S | Medium | **Adopt** in the same issue. |
| Templates in `templates/*.md` | `start_session` takes a prompt; roles live in the supervisor brief | Reusable roles (reviewer, simplifier) as plain files | S | Medium | **Adopt**. Issue filed. |
| `run --keep` recovery, backoff, never re-run the task | `reset_running` then re-prompt every cut-off session | Resume policy is all or nothing; no per-session choice, no grace, no kept PTY shell recovery | M | Medium | **Adapt**. Issue filed. |
| Holder processes so terminals survive core restart | Turn processes die with the server (`KillMode=control-group`); PTY panes die | Agents mid-turn are cut by every deploy | L | Medium | **Skip for now**: re-prompt resume works. Revisit only if restarts stay frequent. |
| Layout/session atomic save, corrupt-file quarantine, timed snapshots | One SQLite file, no rotation | A bad write or migration loses everything | S | Medium | **Adopt** a DB snapshot rotation in the recovery issue. |
| Config as plain files with a generated `AGENTS.md`, live reload, `.error` files | `agentbench.toml` plus DB settings | Settings only editable through the UI; agents cannot change the app's look or defaults | M | Medium | **Adapt**: files for templates, themes, keys, settings; no JS mods. Issue filed. |
| JS mods (GPUI), layouts as JSON, shipped mods | egui, fixed layout, 1 to 6 tiles | Fully scriptable UI | L | Low | **Skip**: unsandboxed code and a big surface. Themes and keys only. |
| `herdr-app shot` (PNG of the window for the agent) | Headless screenshots in tests | Let an agent see the UI it edits | S | Low | **Adopt** in the config issue as `agentbench shot` only if the egui frame can be captured cheaply. |
| Reset hotkey for a broken layout | n/a | Needed once config files can break the UI | S | Low | **Adopt** with the config issue (invalid file keeps last good, `.error` file). |
| Sounds on done and blocked, clickable toast | Desktop notice, Android service, no sound setting | Optional sounds and toast that opens the session | S | Low | **Adopt**. Issue filed (with seen/pinned). |
| tmux-style prefix keys, `keys.json`, command palette | Fixed shortcuts (`SHORTCUTS` in `modals.rs`), F1 help | User-editable keys and a palette | M | Low | **Adapt** in the config issue: `keys.toml` for existing actions. Palette skipped. |
| Spaces per repo, tabs per task, panes | Projects per repo, worktree per issue, session per chat, tile grid | Equivalent. Ours is issue-centred | n/a | n/a | Already have; keep. |
| Worktree and git-changes mods | Worktrees, changes view | Equivalent | n/a | n/a | Already have. |
| `doc.get`/`doc.set` with version compare-and-set | Notes with versions and compare-and-set (#92) | Equivalent | n/a | n/a | Already have. |
| Attach streams, framing, flow control, ghostty VT, resize ownership | vt100 and a screen poll | Smooth terminal for interactive TUIs | L | Low | **Skip**: our sessions are chats, the PTY is for logins and a shell. |
| Windows named pipes, macOS launchd bundle, Windows signing | Cross-platform builds, packaging per OS | Not needed for the server design | L | Low | **Skip**. |
| Telemetry on by default | none | n/a | n/a | n/a | **Skip**. We do not add telemetry. |
| `hc update` with checksum manifest | Release packages | Self-update for a server we run by hand | M | Low | **Skip**: we use `cargo install` and CI releases. |
| Marketplace of plugins | none | n/a | L | Low | **Skip**. |

## What agentbench has that Herdr does not (do not regress)

- An issue tracker per project with states, labels, priority, comments, drag between columns, and
  start-an-agent-on-an-issue creating a named worktree.
- A supervisor chat with judge gate (`judge_session`), merge gate, per-project automation levels,
  risky-action toggles, autopilot loop with caps and stuck nudges, a triage pass across projects.
- Native API agent with a request size budget, picture handling, compaction, a planning watchdog,
  bash-edit steering, model levels (low, medium, high) and provider onboarding.
- Cost and usage per session, context size, titles.
- Versioned project notes, global notes, project groups, hundreds of repos in one board.
- Voice in and out, send_file with previews, a browser tool, a screenshot tool, secrets that agents
  never see.
- Android app and web, with pairing by QR, plus background notification service.
- Transcript as data (tool calls, thinking, notices with severity), not a screen scrape.

Herdr's strength is the opposite: it is agent-brand-agnostic and survives anything because everything
is a terminal. Ours is structured, so we know status for free and can gate merges. The ideas worth
taking are the ones that open agentbench to agents it does not run itself, and to machines it does
not run on.

## Architecture notes for the big candidates

### (a) Status reporting for any CLI agent

Goal: an interactive Claude Code or Codex running in a PTY (ours, or anyone's) shows in the session
list with working, blocked, done, idle and counts toward "waiting for you".

- Add `ExternalAgent` as a session kind: no turns driven by us, status set from outside. Same
  `SessionStatus` plus a new `Done` marker derived from a `turn` counter and `seen_turn`.
- Two inputs, one reducer. (1) The vt100 parser in `terminal.rs` surfaces OSC 7501 through its
  unhandled-OSC callback (check vt100 0.18's `Callbacks`; if it is not exposed, scan the byte stream
  before feeding the parser, 40 lines). (2) `POST /api/sessions/{id}/status` and `abc status --state
  working --kind permission --msg ... [--id child]` for agents that are not in our PTY, authenticated
  by the same token. The environment gives agents `AGENTBENCH_SESSION`, `AGENTBENCH_URL`,
  `AGENTBENCH_TOKEN` already (`agent/mod.rs`), so no new discovery is needed.
- Reducer copied from `status.rs`: records by id, root status, 256 record cap, `clear`, turn bump on
  leaving `working`, "stuck" edge for blocked and error, reset on process exit.
- Waiting list: `blocked` sets the existing waiting marker, desktop notice and header count. Reuse
  the "needs you" path; do not add a second one.
- Do not copy the screen detection manifests. If a vendor has hooks (Claude Code hooks, Codex
  `notify`), document a snippet that calls `abc status`. We print it, we do not edit the user's config.
- Security: a report can only change its own session, and `blocked` never unlocks anything. Say so in
  the docs as Herdr does.

### (b) Remote machines

Today the app can talk to one remote server, and the server runs everything local. The aim is a
local server that also lists sessions that run on other machines.

- **Transport.** Do not expose more HTTP ports. Add `agentbench connect`: reads HTTP/1.1 requests
  from stdin and writes responses to stdout against the local server on that machine
  (`ssh spark agentbench connect`), the same trick as `hc connect`. Implement it as a hyper
  client connection over a duplex of the child process's pipes; the SSE or long-poll feed rides the
  same pipe. The token is not needed on the pipe because SSH authenticated the user, but the local
  server still forwards it. If a machine already exposes HTTPS with a token, a `url` machine kind
  reuses today's remote-server code.
- **Model.** A `machines` table: name, kind (`ssh`, `via`, `url`), target, options, last seen,
  version. `machine add host --name spark [--session name] [--via '<cmd>'] [--install]`.
- **Federation.** The local server holds one multiplexed connection per machine, pulls `/api/overview`
  and the change feed, and re-serves the remote rows with a `machine` field and ids prefixed
  `spark/`. Projects, issues and worktrees stay in the remote DB (integer ids collide across DBs, so
  the prefix is required; session ids are UUIDs and survive). Actions on a prefixed id are proxied.
  A lost link dims the rows (Herdr keeps the last state visible but dimmed and disables input) and
  retries with backoff, with a 30 s recheck.
- **Where code runs.** A session started "on spark" is a normal session on spark's server, on spark's
  checkout, with spark's GPUs. We do not copy `hc exec` and `hc file` unless a case needs an agent
  here to touch files there; today the right answer is to run the agent there.
- **Install.** Copy the server binary when OS and CPU match, else ask the user. Never overwrite
  silently; show the command.
- **Trust.** Write down what Herdr's `security.md` says: adding a machine gives local clients the
  remote account's access.
- Open question for the issue: show remote supervisors and autopilot (probably one supervisor, the
  local one, driving remote workers through `start_session` with a machine argument).

### (c) Parent and child agents

agentbench already has `sessions.parent`, supervisor-only `start_session`, `watch_session`,
`wait_for_session`, routed questions, digests. Missing, compared with Herdr:

- **Any session can parent.** Today only the supervisor starts workers. Allow a worker to start a
  child when its brief or the project allows it (`allow_children`), with a depth cap.
- **Lifetime.** Child ends when its parent session is closed or stopped, unless started with
  `outlive_parent`. No lifetime rule ties a child to its parent today. Test: close the parent, the
  child stops; with the flag it stays and its `parent` is cleared.
- **Wait and report back.** Worker tool `wait_children(any|all, timeout)` returns a structured
  result per child (`status`, `exit`, `running`, `timeout`) and comes back after at most 180 s
  with `reason: timeout` and the ids, never blocking a turn forever (Herdr's check-in).
  `report_to_parent(text)` posts a tagged note, `<ab from="id title">...</ab>`, which the parent
  treats as a child report, not as the user's instruction (Herdr's `<hc from=...>` convention).
- **Guards.** Sending to a session that is waiting on a question is refused unless forced, so a
  supervisor cannot answer a worker's `ask_user` by accident. `interrupt` returns what changed.
- **Fit with the supervisor.** The supervisor stays the root with extra authority (judge, merge,
  close). Children of workers report to their parent worker, and a worker's `DONE` still goes to
  the supervisor through the existing `PARENT=` rule. Notes to any parent wait until it is idle (the
  #209 mechanism, generalised from `supervisor` to `queue_system`, which already exists as a field).

### (d) Schema and skill endpoints

- OpenAPI 3.1, generated from the Rust types (`utoipa` or `schemars` plus a route table), served at
  `GET /api/schema` and printed by `agentbench schema`. A checked-in `docs/api/openapi.json` and a
  test that fails when it is stale, and a second test against `docs/api/released.openapi.json` that
  fails on a breaking change (removed route or field, new required request field, narrower input),
  copied from Herdr's approach. Error bodies get a stable `kind` and `hint` like theirs.
- `agentbench --skill` and `GET /api/skill` print a manual for outside agents: environment variables,
  start and message sessions, wait, events, status reporting, the `abc` commands, rules for
  children. The text lives in one file embedded with `include_str!`, and a tiny
  `skills/agentbench/SKILL.md` stub only says to run it. A test checks every command named in the
  skill exists in `--help`.

### (e) `abc`, a small CLI over REST

A subcommand set on the existing binary (`agentbench ls`, with `abc` as a symlink or alias), no new
server logic. `ls`, `get <id|title>`, `read <id> [--lines N|--since]`, `send <id> text`,
`wait <id> [--children] [--timeout 180]`, `events [--session] [--project] [--parent-of]`,
`start`, `stop`, `status` (the report command), `schema`, `--skill`. JSON when piped, text on a
tty, `--json` to force, stable error kinds and exit codes (3 not found, 9 blocked, 124 timeout,
as Herdr). Uses `AGENTBENCH_URL` and `AGENTBENCH_TOKEN`. Titles resolve to ids like Herdr's names.

### (f) Plain-file config

`$AGENTBENCH_HOME/config/` with `settings.toml`, `keys.toml`, `themes/*.toml`, `templates/*.md`,
and a generated `AGENTS.md` that describes the folder and how to edit it. Live reload, invalid file
keeps the last good one and writes `<name>.error`. The DB keeps secrets and runtime state; files hold
what a person or an agent should be able to read and change. No JS and no layouts: egui layouts stay
in code, themes change the palette within the egui_bench grammar.

### (g) Recovery after a restart or reboot

Today: every running or waiting session becomes stopped and is re-prompted with a fixed prompt. That
is right for chat agents, wrong for some (a session the user stopped on purpose, an external agent we
cannot re-prompt, a PTY shell). Rules to adopt from Herdr: per-session `keep`; never re-run a task
that finished or was killed; an exit with a result is final; a PTY pane gets a fresh shell in its last
folder; a grace delay and capped backoff when resume fails; a `reason` on stopped (`server_stop`,
`host_shutdown`, `stopped_by_restart`, `exited`, `killed`); externally reported agents are marked
`lost` and wait for their own report. Plus snapshots of the SQLite file (a copy every 15 minutes at
most, keep 48, keep 3 copies of a file that failed to open), written atomically.

### (h) Other things worth taking, and what to skip

- Notifications with sound and a toast that opens the session; `seen_turn` so done clears on every
  client.
- Templates for roles (reviewer, simplifier, implementer) as plain markdown files.
- A security page that states the trust model.
- Skip: JS mods, layout JSON, screen detection, holders, telemetry, marketplace, self-update.

## Licence

All three repos are Apache-2.0. Ideas, protocol names and small snippets can be reused. agentbench is
GPL-3.0-only, which can include Apache-2.0 code (one-way compatible). If we copy more than a few
lines (the reducer in `status.rs`, `Status::encode`, the OpenRPC compat test logic, the `SKILL.md`
text), keep the file's notice, say it is derived from herdr-core in a comment at the top of that
file, and add a `NOTICE` file at the repo root listing the repo, the commit and the paths used, plus
the Apache-2.0 text under `licenses/`. We plan to rewrite, not vendor: nothing here should be
vendored as a crate, since `herdr-core` is a binary and its terminal stack (ghostty, holders) is not
what we need. Do not copy the OSC 7501 name as a trademark claim: it is a number in the private-use
range, fine to reuse, and we cite where it came from.

## Order of work

Umbrella #290.

1. #281 remote machines (stdio transport, federation).
2. #282 status protocol for external agents.
3. #283 schema and skill endpoint, then #284 the `abc` CLI (the first user of both).
4. #285 parent and child rules.
5. #286 recovery semantics and DB snapshots.
6. #287 templates, #288 seen state and notifications, #289 plain-file config.
