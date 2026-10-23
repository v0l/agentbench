# Changelog

All notable changes to Foreman are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- Settings are split into four tabs: general, speech, agent and titles.
- The CUDA build loads the CUDA runtime when the window starts instead of linking it, the way
  WaveShark does. One `--features cuda` build runs on a machine with no NVIDIA runtime and reads
  speech on the CPU there. `.cargo/config.toml` builds the kernels as PTX for compute 8.0, which
  the driver finishes for any later card.
- Voice input offers the larger Whisper models: small, medium and distil medium in English, the
  multilingual tiny to medium, and large v3, large v3 turbo and distil large v3. They run on an
  NVIDIA card when foreman is built with `--features cuda`, which the default build is not.
- Foreman is licensed under the GPL-3.0 instead of the MIT license.

### Added
- A grid of chats: the keys beside **settings** tile the chat area as 1, 2x1, 1x2, 2x2 or 3x2.
  One tile is focused (click in it to focus it) and opening a chat from the list replaces the
  chat in that tile, so there is nothing to assign by hand. Opening a chat that is already on
  screen only focuses it. Each tile keeps its own draft; the grid, its chats and the focused
  tile are remembered between runs.
- Voice input: a speak key beside send in the composer records from the microphone and writes what you said into the message. Whisper runs on this machine (tiny, base or small, English, downloaded the first time) or on any server with an OpenAI style `/v1/audio/transcriptions`, chosen in settings.
- The `foreman` agent can search the web with `web_search` and read a page with `web_fetch`, which
  turns HTML into markdown, reads long pages in pieces, and downloads any file with `save_to`.
  Pass `prompt` to get the title model's answer to a request about the page, such as a summary or
  one fact, instead of the page.
- More tools for the `foreman` agent:
  - `bash` takes `background`, with `bash_output` and `bash_kill`, to run a dev server, watcher or long build while it carries on.
  - `start_session`, `send_message` and `wait_for_session` let one agent hand an issue or prompt to another session, steer it and wait for the result. `session_status` is the cheap check.
  - `start_session` can start in another project (`list_projects`) after the user allows it in the chat: once, always for that project, or always for any project. `DELETE /api/settings/cross-project` takes the standing permissions back.
  - `get_issue` reads an issue with its comments, and `file_issue` and `update_issue` take labels, priority and state.
  - `list_worktrees` and `create_worktree`, `todo_write` (drawn as a checklist in the chat), `screenshot` (headless Chrome, Chromium or Edge), `notify_user`, and `remember` / `recall` for per-project notes that open every later session.

- A status line under each chat's header: the server and model the agent is using, how full its
  context is against the model's window (amber from 60%, red from 80%), and how much of the input
  the prompt cache served. Each agent reports what it can: `claude` and `pi` name their model, `pi`
  its provider, `claude` and `codex` the window, and the `foreman` agent its server and model,
  following `/model` and `switch_model` as they change.
- Auto-compact for the `foreman` agent: once a chat's context passes **compact at** (80% by
  default, 0 to turn it off) foreman summarises it and starts the agent afresh from the summary,
  as `/compact` does. The window is whatever the model's server reports (`/models`, llama.cpp's
  `/props`, Ollama's `/api/show`), or 128k when it does not say; there is no setting for it.
  `pi`, `claude` and `codex` compact themselves.

- Pair a phone: settings has a **pair a phone** section. Give it the address a phone reaches the
  server at and it shows a QR code holding `foreman://pair?url=…&token=…`. Opening that link in
  the Android app connects it, and pasting one into the address field fills in the address and
  token.
- On a narrow window (a phone) the project list is a drawer behind a hamburger, the header
  stacks onto three rows, dialogs fit the screen, and the app keeps clear of the status and
  gesture bars.

- The main tabs read **agents** and **issues**, not the open chat's title.

- Android notifications: a chat that **needs you** or **failed**, and one that **finished**, raise a
  notification even with the app closed. A small foreground service holds one long poll open to the
  server (a request every 25 seconds, answered at once when something changes), backs off while the
  server is away, and restarts after a reboot; a 15 minute job looks as well when the system stops
  the service. Turn it off with settings → general → chats → alerts. The Android build is now
  `packaging/android/build.sh`, which adds the Kotlin service (compiled with kotlinc, no Gradle).
- Android: a launcher icon.

### Fixed

- Android: the app kept nothing between launches, so the paired server was forgotten. Its settings are now saved in the app's private files.
- A long draft scrolls inside the composer instead of growing it until the send, speak and attach keys are pushed out of the window.
- A long speech model name no longer pushes the dropdown and its help icon out of the settings dialog.
- Pasting or attaching a picture no longer takes the cursor out of the composer.
- A worktree that was deleted from disk no longer keeps its **gone** marker after you delete its
  last chat. The marker stays only while a chat there still needs reading.

## [0.1.1] - 2026-10-23

### Changed
- The icon is a face in a hard hat.

## [0.1.0] - 2026-10-22

First release.

### Added
- The `foreman` window and server in one binary, and `foreman-server` for headless machines.
- Projects, worktrees, agent chats and an issue tracker kept in one SQLite file.
- `.deb`, `.msi` and `.dmg` installers and `.tar.gz` / `.zip` archives with the Foreman icon.
