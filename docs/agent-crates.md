# Agent crates

The agent is split into two library crates in this repo, plus the pieces that stay where they are.

| crate | licence | depends on | holds |
|---|---|---|---|
| `crates/agent-core` | MIT | no egui, no agentbench types | the UI-free agent kit that used to live in `egui_bench::agent` |
| `crates/agent-native` | GPL-3.0-only | `agent-core`, `agentbench-core`, `egui_bench` (only `voice::audio`) | the native agent loop, its tools, and the contract the runner drives agents through |
| `crates/server` | GPL-3.0-only | both | runner, store, API, autopilot, and the external CLI agents |
| `egui_bench` (branch `agent-core`) | MIT | `agent-core` | chat, pane, settings and `Agent<H>`/`Host`/`serve`, re-exporting `agent-core` under `egui_bench::agent` |

`agent-core` is MIT so MIT and GPL apps can use it. `agent-native` is GPL like the rest of agentbench.

## egui_bench `src/agent` (about 10.9k lines)

| item | lines | goes to | notes |
|---|---|---|---|
| `code.rs` | 1967 | core | language servers behind `code_outline`, `code_find`, `code_diagnostics` |
| `config.rs` | 165 | core | `Config` (url, model, key, steps) read and written as a file |
| `grants.rs` | 411 | core | shared-folder grants and the ask/decide queue |
| `http.rs` | 53 | core | shared reqwest client; user agent is now `agent-core/<version>` |
| `jobs.rs` | 364 | core | background `bash` jobs |
| `local.rs` | 444 | core | the built-in tool dispatcher (`NAMES`, `run`). Not a model picker, despite the issue text |
| `lsp.rs` | 819 | core | LSP client |
| `mcp/` | 1748 | core, feature `mcp` | MCP client |
| `sandbox.rs` | 184 | core, feature `sandbox` | `nono` sandbox; stubs when the feature or the OS is missing |
| `skills.rs`, `skillset.rs` | 444 | core | skills.sh search, `SKILL.md` discovery |
| `spawn.rs` | 369 | core | confined process spawning |
| `todo.rs` | 120 | core | `todo_write` |
| `toolkit.rs` | 327 | core | `Tool`, `Toolkit` (tests no longer need an egui context) |
| `tools.rs` | 336 | core | `Spec`, schema helpers, `GUIDE`, `offered`, `list` |
| `web.rs` | 1146 | core | `web_search`, `web_fetch`, `Reader` trait |
| `workspace.rs` | 444 | core | file tools confined to a folder |
| `mod.rs`: `Reply`, `Ask`, `Desk`, `Services`, `Cx`, `Welcome`, `Backend`, `runtime`, `async_trait`, `CancellationToken` | about 190 | core | `Desk` holds a `ring` callback instead of an `egui::Context` |
| `mod.rs`: `Poll`, `Handled`, `Host`, `Pending`, `Agent<H>`, `serve` | about 100 | UI | all take an `egui::Context` |
| `chat.rs` | 716 | UI | transcript state and the model loop; stays with the pane that draws it |
| `pane.rs` | 544 | UI | |
| `settings.rs` | 64 | UI | |

Boundary types that cross from UI to core: `Desk` (built from `Desk::new(ring)`, `egui_bench::agent::desk(&ctx)` does that for a context), `Cx`, `Services`, `Backend`, `Welcome`, `Reply`, `Ask`, `Config`, `Grants`, `Workspace`. Nothing crosses the other way except the re-exports.

## agentbench `crates/server/src/agent` (about 12.2k lines)

| item | lines | goes to | notes |
|---|---|---|---|
| `native.rs` | 6210 | native | the loop, tool specs, model config (`Model`, `Level`, `Shared`), `Native`, `Pace` |
| `browser.rs` + `browser/*.js` | 1978 | native | Chrome DevTools driver |
| `code.rs` | 81 | native | maps `agent_core::code::Health` onto `agentbench_core::LspHealth` |
| `files.rs`, `replace.rs`, `search.rs` | 1319 | native | file tools |
| `limits.rs`, `secrets.rs`, `steer.rs`, `progress.rs` | 673 | native | bash limits, secret placeholders, edit-by-shell steering, read-only watchdog |
| `media.rs`, `screenshot.rs` | 812 | native | pictures, sound and video for `read_file`; screenshots |
| `mod.rs`: `Context`, `Attached`, `Steer`, `Turn`, `Event`, `Decoder`, `Io`, `Api`, `Agent`, `Folding`, `Folded`, `resolve`, `brief` and its prompts, test helpers | about 520 | native | the contract between a runner and an agent; needs only `agentbench-core` |
| `mod.rs`: `defaults()` | 3 | server glue | builds `Native` |
| `process.rs`, `command.rs` | 438 | server glue | external CLI agents (`claude`, `codex`, `JsonCommand`); use the contract and `agent_native::tree::kill_tree` |

## agentbench items outside `agent/` that native needs

| item | from | to | how the server keeps using it |
|---|---|---|---|
| `fold.rs` | server | native `fold` | `pub use agent_native::fold` |
| `http.rs` | server | native `http` | `pub use agent_native::http` |
| `quota.rs`: `Price`, `Prices`, `Clock`, header and body parsing, `fetch`, `Quotas`, `Hint` | server | native `quota` | server `quota.rs` re-exports them and keeps `paused_until`, `set_paused`, `servers`, `poll_once`, `spawn_poll` (store and runner) |
| `tools.rs`: `PROPOSAL`, `required` | server | native `gate` | `pub use` from `tools.rs`; `StoreApi` stays in the server |
| `autopilot.rs`: `JUDGING_RULES` | server | native `brief` | `pub use` from `autopilot.rs` |
| `titles::Titles` (web_fetch answers) | server | trait `agent_core::web::Reader` | `Native::with_reader`; `Titles` implements `Reader` |
| `speech::Speech` (Whisper for sound files) | server | trait `agent_native::Ears` | `Native::with_ears`; `Speech` implements `Ears` |
| `titles::fake`, `quota::fake` test servers | server | native `testing` (feature `testing`) | server tests use them through a dev-dependency |

## Tests

- Tests move with their code. Tests that need the store (`a_proposal_asks_the_user_and_runs_only_what_they_allowed`, `another_project_is_entered_only_after_the_user_says_so`, the runner-driven quota poll tests) stay in the server.
- `agent-native` enables `testing` for its own tests. The server enables it as a dev-dependency so `Patience::default()` still skips the context probe against stub servers.

## Wiring

- `egui_bench` (branch `agent-core`) depends on `agent-core` by git (`v0l/agentbench`, `master`).
- agentbench depends on `egui_bench` by git (`agent-core` branch) and patches `agent-core` back to `crates/agent-core`, so both sides build from this checkout:

```toml
[patch."https://github.com/v0l/agentbench"]
agent-core = { path = "crates/agent-core" }
```

- Both git dependencies go away when the crates are published and the versions are bumped.
