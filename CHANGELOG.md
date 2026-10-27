# Changelog

All notable changes to Agentbench (called Foreman before 0.2.0) are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `find_skills` for the built in agent: searches the skills.sh directory and gives each result's install count, page and install command, so an agent short of know-how can look for a skill. It only searches; the brief tells the agent to ask you before installing one.
- `agentbench chat`, a terminal chat with any agent, for odd jobs that belong to no repository. It opens in your home folder like a chat from the window, shares the server, and shows the same transcript: markdown, tool calls (ctrl+o opens their output), thinking, questions you answer with the arrow keys, and the model, context fill and cost in the status line. `-c` carries on with the latest chat, `/resume` picks an older one, and the same slash commands work (`/model`, `/thinking`, `/compact`, `/title`, `/new`, `/close`). `--agent`, `--model` and `--project` choose where it goes, and `--url` with `--token` uses a server on another machine. Leaving does not stop the agent.

## [0.4.0] - 2026-10-24

### Changed
- The window and the phone take the speech models and the read-aloud voices from the agentbench server (`GET /v1/audio/models` and `/v1/audio/voices`) instead of a list built into the app, so a model or voice added to the server shows up without a new app release. A choice the app has never heard of is kept. This device's own Whisper still offers only the models it can run.
- Read aloud uses Phonon instead of Kokoro: it reads numbers, versions, dates, units and URLs the way a person would ("0.3.2" is "zero point three point two", not three pauses), and in a test of agent replies it made no word errors where Kokoro made 6%. The model is about 100 MB, run through candle (`egui_bench::speech::phonon`, ported from gradium's `ptts` and checked against it step by step), and on a GPU it makes 21 seconds of speech in 0.4. It streams a frame at a time, so the first sound comes almost at once. There are 25 voices, American and British; pick one under settings, voice. A voice chosen before (a Kokoro voice) falls back to the default, Hadley.
- When the native agent reads a sound file it gets the words in it, transcribed by the server's Whisper, instead of the sound. Most models behind a router take no audio, and sending some made the router refuse the whole turn. The agent is told it has the words and not the voice.

### Fixed
- A turn that the model server refused part way, with an error sent inside a stream that had already started, now ends with that error instead of in silence.

## [0.3.2] - 2026-10-24

### Added
- The Mac release build runs voice input and read aloud on the Apple GPU through Metal (feature `metal`, from egui_bench 0.6.1). On an M1 Pro, Kokoro makes 7.3 s of speech in 2.3 s, against 3.4 s on the processor.
- The native agent reads the standing instructions in `~/.agents/AGENTS.md`.

### Fixed
- The Windows release build with CUDA: it is built on Windows Server 2022, since CUDA 12.8 refuses the newer Visual Studio on the latest runner. 0.3.1 has no release because of this.

## [0.3.1] - 2026-10-24

### Changed
- The Linux x86_64 and Windows release builds have CUDA in them, as waveshark's do: on a machine with an NVIDIA card and the CUDA 12 runtime, voice input and read aloud run on the GPU. They still start, and run speech on the CPU, on a machine with no card. The arm and Mac builds are unchanged.

### Fixed
- The Android app builds again: 0.3.0 has no APK because of this.

## [0.3.0] - 2026-10-24

### Added
- **Code tools** for the native agent, built on language servers (Rust, Go, TypeScript/JavaScript, Python, C/C++, Zig, Dart and Lua out of the box, any other from `[[code.lsp]]` in `agentbench.toml`). The languages a project uses are detected from its `Cargo.toml`, `go.mod`, `package.json` and the like, and each server runs from the outermost folder that language was found in, so a repo with a Go backend and a web frontend gets both. `agentbench-server code [folder]` shows what was found and what is missing. `code_outline` breaks a file or folder into its units, `code_find` gets a function, struct, enum or other unit by exact name with its source and every unit that uses it, and `code_diagnostics` lists a file's errors. After the agent edits a file, the server's errors in it are added to the chat as a message from the harness, so it sees them without building.
- The chat's status line lists the project's language servers, each in its own colour: green when ready, amber while starting or indexing, grey when not installed, red when it failed. Hover for the folder each runs in and, for a missing one, the command that installs it. The server is shown first, then the model and context, so a narrow line drops the cache and the server name before it drops these.
- **Read aloud.** Every reply has a "read aloud" link under it. The agentbench server says it with Kokoro, a voice model ported from waveshark's `tts` crate, and streams it back a sentence at a time while the window plays it, so the first words start in about a tenth of a second on a GPU instead of after the whole reply is made. Press the link again to stop. A server too old to read aloud now says so instead of doing nothing. Markdown is read as words: links as their text, code blocks and URLs left out. Pick the voice under settings, voice, read aloud. It is not available in the browser build.
- The server has a text to speech endpoint like the one for speech to text: `POST /v1/audio/speech` takes OpenAI's `{"input", "voice"}` and answers a WAV, and `GET /v1/audio/voices` lists the voices. `"response_format": "pcm"` streams raw 16 bit samples (rate in `x-sample-rate`) a sentence at a time as they are made, and `GET /v1/audio/status` says whether speech runs on the GPU or the CPU. Both need the token, so a phone or another window can use a remote server. The model (about 330 MB) and each voice are fetched from Hugging Face the first time, into `models/kokoro` in the data folder (an existing folder from waveshark can be copied or linked there), and stay loaded, on the GPU when the server was built with CUDA.
- The agent's current checklist is pinned above a chat, so what it is working on shows without reading the transcript. Click its header to fold it; it starts folded on a phone or in a tile. Claude's `TodoWrite` is drawn too.

### Changed
- Settings, voice now shows where speech runs: the server's read aloud and voice input, and this device's own Whisper, each on the GPU or the CPU.
- Speech to text and text to speech now live in `egui_bench` 0.6 (`voice` for the helpers and an OpenAI audio client, feature `speech` for the Whisper and Kokoro models), so the other bench apps can use them. The `agentbench-speech` crate is gone, and so are the audio helpers in `agentbench-core`. Nothing changes for you.
- Voice input waits for 3 seconds of silence before it ends a take, up from 1, so a pause to think does not cut you off.

## [0.2.0] - 2026-10-23

### Changed
- **Foreman is now Agentbench.** The binaries are `agentbench` and `agentbench-server`, the crates `agentbench-*`, the settings `AGENTBENCH_*`, the config and database `agentbench.toml` and `agentbench.db`, the link scheme `agentbench://`, and the Android package `io.v0l.agentbench`. The built-in agent, which shared the app's name, is now called **native** (existing chats are moved to it).
- Installs from before the rename keep working: `FOREMAN_*` variables are still read (and agents still get both names in their environment), an existing `~/.local/share/foreman` folder with `foreman.db` and `foreman.toml` is used until you rename it, the window copies its saved settings across, and `foreman://` pairing codes and links still open. The old systemd unit and the Android app's data are not carried over: the Android package id changed, so it installs beside the old app, and pairing again is needed.

## [0.1.2] - 2026-10-23

- A built-in **chats** project, first in the list, for chats that belong to no git repository: parse files, run skills, odd jobs. It has a "new chat in ~" row, and its chats start in your home folder with a shorter brief (no branches or worktrees). It is made by the server at startup, cannot be removed, is never scanned for worktrees, and issues cannot be filed against it.

- A bash command is always shown whole and wrapped under the tool's name, whether the row is open or not, since it is the riskiest thing an agent does. It used to be cut to one line.

- Releases include a signed Android APK (`foreman-<tag>-android-arm64.apk`), built by the release workflow with `packaging/android/build.sh`. A phone that has a debug build installed has to uninstall it first, because Android will not change an app's signing key.

### Fixed
- Long chats are much faster to draw, most of all on a phone. Only the messages in view (and a
  screenful either side) are laid out; the others are given the height they were last measured at.
  Resizing the window or changing the text size keeps the old heights, scaled to the new width, as
  a guess for what is out of sight, so only the visible messages are laid out again and resizing
  is near-instant. The transcript is no longer cloned every frame, and the markdown cache is only
  swept when it has grown large. A 400-message chat took 7 ms to draw when idle, 16 ms while
  scrolling and 16 ms while resizing, on a desktop and in the phone layout; it now takes 0.1 ms,
  0.4 ms and 0.4 ms. Opening a long chat still lays every message out once (about 20 ms).

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
- Android: tapping a notification opens the chat it is about.
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
