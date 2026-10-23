# Changelog

All notable changes to Foreman are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- A grid of chats: the keys beside **settings** tile the chat area as 1, 2x1, 1x2 or 2x2.
  Opening a chat puts it in the first tile, pushes the others back one place and drops the
  last out of view, so there is nothing to assign by hand. Opening a chat that is already
  on screen only focuses it. Each tile keeps its own draft; the grid and the chats in it
  are remembered between runs.

## [0.1.1] - 2026-10-23

### Changed
- The icon is a face in a hard hat.

## [0.1.0] - 2026-10-22

First release.

### Added
- The `foreman` window and server in one binary, and `foreman-server` for headless machines.
- Projects, worktrees, agent chats and an issue tracker kept in one SQLite file.
- `.deb`, `.msi` and `.dmg` installers and `.tar.gz` / `.zip` archives with the Foreman icon.
