# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tag-triggered release automation. Pushing `vX.Y.Z` runs the full suite on
  Linux and macOS and publishes the GitHub release with notes taken from that
  version's changelog section — but only after an identity gate has confirmed
  that the tag, `Cargo.toml`, `Cargo.lock` and `herdr-plugin.toml` all name the
  same version and that the changelog section for it exists and is not empty.
  The manifest version is the one the marketplace displays and the one easiest
  to forget, so it is checked explicitly.
- An advisory upstream canary. Once a day it resolves one exact herdr `master`
  commit, fetches the API schema herdr generates from its own types at that
  revision, and checks that the three methods pulse calls, the parameters it
  sends, and the snapshot fields it reads are all still there. It is scheduled
  and manual only, it is not a required check, and a red canary is a signal to
  read herdr's recent changes rather than a reason to hold a pull request.

### Changed

- Series are kept per herdr session, and every interface says which session a
  series belongs to. Workspace ids, pane ids and `state_change_seq` are all
  scoped to one run of the server, so buckets recorded under two sessions were
  never comparable — and appending them into one sparkline claimed an unbroken
  watch across a restart nobody watched across. The store now holds one series
  per (workspace, session); the pane gains a `session` column giving the time
  that session started listening, parenthesised when it is not the session
  running now and `?` when it could not be established; and `--json` carries the
  fingerprint, start time and `is_current` per workspace plus the live session at
  the top level.

  The earlier session's observed buckets are still drawn as the bars they are.
  The seam is marked, never blanked: blanking it would say nobody was watching
  when somebody was, which is the error this plugin exists to refuse.

  Two consequences worth stating. A badge is one series against one
  session-scoped id, so only the live session's rows are badge targets — a badge
  pushed to an id inherited from an ended session can land on a different
  workspace entirely. And a first sample after a restart no longer counts a
  transition per agent: `state_change_seq` starts over, and comparing it across
  sessions manufactured churn out of a reset counter.

  herdr publishes no session identity — verified against a live 0.8.0 server's
  own schema, where `SessionSnapshot` has no session id, start time or boot
  counter, and every `server.*` method is an action rather than a read — so pulse
  fingerprints the socket it read the snapshot from. That can report two sessions
  where there was one, which splits a history; it cannot report one where there
  were two, which would join two incomparable series. A history file written by
  0.1.0 keeps its buckets as one unattributable session rather than being claimed
  by whichever session happens to be running now.
- History survives a reused workspace id. herdr's ids are session-scoped, and a
  workspace that came back under an id somebody else had been using lost every
  recorded bucket. The store now keys a series on the workspace's checkout path
  (`workspaces[].worktree.checkout_path`), which means the same thing in every
  session, so a renamed workspace, a workspace renumbered by a fresh server, and
  two workspaces that swap ids all keep their own history.

  The correctness rule is unchanged: where identity cannot be established, drop
  rather than guess. A workspace herdr reports no worktree for has no durable
  key, so the id and the label together are still all the evidence there is, and
  a label that changes under an id still drops the buckets. Two workspaces on one
  checkout path make that path ambiguous, so it is not recorded as a key for
  either of them and neither can inherit the other's minutes when the checkout
  becomes unambiguous again. A snapshot that reports one workspace id twice
  contradicts itself and records nothing, since both observations claim the
  handle a badge is pushed to. A workspace herdr only starts reporting a worktree
  for partway through a session keeps the buckets recorded before it: the first
  sample carrying a path adopts that ring and stamps the path onto it. (A file
  written by 0.1.0 predates the session field too, so it is kept as one
  unattributable watch instead — see the session entry above.)
- `min_herdr_version` is now `0.8.0`, up from `0.7.5`. The old floor was reasoned
  from when the socket APIs pulse calls first appeared; it was never exercised
  against a 0.7.x server. 0.8.0 is the latest stable herdr and the only version
  pulse has been developed and verified against, so the manifest now states a
  tested claim rather than an inferred one. **Installing on herdr 0.7.5 through
  0.7.x, which the manifest previously permitted, will now be refused.** If you
  are on one of those and pulse worked for you, say so on the issue tracker and
  the floor can come back down with evidence behind it.

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
