# Changelog

All notable changes to Foreman are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- A grid of chats: the keys beside **settings** tile the chat area as 1, 2x1, 1x2 or 2x2.
  One tile is focused (click in it to focus it) and opening a chat from the list replaces the
  chat in that tile, so there is nothing to assign by hand. Opening a chat that is already on
  screen only focuses it. Each tile keeps its own draft; the grid, its chats and the focused
  tile are remembered between runs.
- Voice input: a speak key beside send in the composer records from the microphone and writes what you said into the message. Whisper runs on this machine (tiny, base or small, English, downloaded the first time) or on any server with an OpenAI style `/v1/audio/transcriptions`, chosen in settings.
- The `foreman` agent can search the web with `web_search` and read a page with `web_fetch`, which
  turns HTML into markdown, reads long pages in pieces, and downloads any file with `save_to`.
  Pass `prompt` to get the title model's answer to a request about the page, such as a summary or
  one fact, instead of the page.

## [0.1.1] - 2026-10-23

### Changed
- The icon is a face in a hard hat.

## [0.1.0] - 2026-10-22

First release.

### Added
- The `foreman` window and server in one binary, and `foreman-server` for headless machines.
- Projects, worktrees, agent chats and an issue tracker kept in one SQLite file.
- `.deb`, `.msi` and `.dmg` installers and `.tar.gz` / `.zip` archives with the Foreman icon.
