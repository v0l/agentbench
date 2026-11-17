# Agent rules

## The API is additive only

The app and the server are installed separately, so an old app talks to a new server and the other way round.

- New response fields get `#[serde(default)]`. New request fields are optional.
- Never rename or remove a field or an enum variant, and never change a field's type.
- Enums the app decodes keep a `#[serde(other)] Unknown` variant.
- A new endpoint may be missing on an older server. The app treats a 404 on a read as "not supported", not as an error.
- Record the new shape: run `RECORD_API_FIXTURES=1 cargo test -p agentbench-server --lib the_recorded_api_fixtures` after bumping `API_VERSION`, which writes `crates/core/tests/fixtures/api/v<N>/`. Keep every older version's fixtures.
- `crates/core/tests/compat.rs` decodes every recorded fixture with today's types and fails on any removed, renamed or retyped field. The failure message points back here.
- A break that cannot be avoided bumps `API_VERSION` and `MIN_CLIENT_API` in `crates/core/src/lib.rs` on purpose, and the PR says so. The app then shows an out-of-date banner for that workspace.
