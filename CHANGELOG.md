# Changelog

All notable changes to Foreman are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
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

## [0.1.1] - 2026-10-23

### Changed
- The icon is a face in a hard hat.

## [0.1.0] - 2026-10-22

First release.

### Added
- The `foreman` window and server in one binary, and `foreman-server` for headless machines.
- Projects, worktrees, agent chats and an issue tracker kept in one SQLite file.
- `.deb`, `.msi` and `.dmg` installers and `.tar.gz` / `.zip` archives with the Foreman icon.
