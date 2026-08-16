# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-16

### Added

- A background sampler that records every agent's lifecycle state from herdr's
  `session.snapshot` on an interval, into a bounded per-workspace ring of
  one-minute buckets.
- A sidebar badge per workspace: an eight-column sparkline over the last 64
  minutes, plus the current state, delivered through herdr's workspace metadata
  tokens.
- An activity pane (`--once`, `--watch`) showing the full retained history, the
  current state, and how long that state has held.
- `--json` for scripting. Gaps are `null` and observed-quiet buckets are `0`, so
  the distinction survives into machine-readable output.
- `--setup` / `--setup-rollback`, which splice the plugin's tokens into the
  user's `config.toml` with a backup and an automatic restore if herdr rejects
  the result.
- `--forget`, to delete the recorded history.

### Notes on the first release

Two behaviours of herdr 0.8.0 that are undocumented were found by testing
against a live server, and both shaped the design:

- **Token values are whitespace-trimmed, and an all-whitespace value deletes the
  token.** The "not observed" glyph is therefore a printing character (`╌`)
  rather than a blank. With a blank, the newest columns of a sparkline were
  silently stripped, so the series stopped lining up with the present, and a
  workspace nobody had been watching lost its badge at exactly the moment the
  gap most needed showing.
- **One `workspace.report_metadata` call may set and clear several tokens at
  once**, so both the badge push and the disable sweep cost one round trip per
  workspace rather than one per token.

Both are written up in `docs/herdr-protocol.md`.

[Unreleased]: https://github.com/moneycaringcoder/herdr-pulse/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/moneycaringcoder/herdr-pulse/releases/tag/v0.1.0
