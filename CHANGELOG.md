# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `--forget` now stops a live sampler before deleting its history and restarts
  it under the same owner afterward. The daemon can no longer recreate deleted
  workspace labels, checkout paths and activity from its in-memory copy on the
  next sampling cycle. A stopped sampler stays stopped, and success is printed
  only after deletion and any required restart both complete.
- Runtime state is now owned per resolved Herdr socket pathname. The default
  socket adopts the exact legacy state root and supervisor label in place and
  records that pathname atomically so later `HOME`/XDG changes cannot redirect
  it; ambiguous existing state is refused rather than guessed. Named sockets use
  collision-free full-hex directories under `sessions/` and distinct
  systemd/launchd labels. History, lifecycle markers, status, badge cleanup,
  disable and forget can no longer cross between named sessions.
- Sampler startup now uses retained Unix owner flocks and serialized lifecycle
  control flocks. Concurrent same-session starts produce one owner, one
  atomically published PID and one history writer; stale PIDs are diagnostic
  only, crashes release ownership automatically, and parent/supervisor startup
  waits boundedly for ownership and PID readiness.
- Sidebar setup and rollback are now atomic transactions. Pulse keeps exact
  before/after recovery metadata, restores only a config Herdr actually rejected,
  distinguishes launch failures from validation failures, and refuses rollback
  without writing when later user edits would be overwritten.

## [0.1.1] - 2026-08-23

### Added

- Every `--json` document now carries `"schema_version": 1`, and
  `docs/json-schema.md` is the written contract: every field's type, unit and
  meaning, and `null` as “not observed” kept distinct from `0` as “observed and
  quiet” in every bucket array. A test pins all 54 key paths with the type and
  nullability under each, so a key that is removed, renamed or moved, or a value
  that changes type, fails the suite before it reaches a consumer's script.
- Optional `--supervise` and `--unsupervise` verbs install or remove a systemd
  user unit on Linux or a launchd user agent on macOS. The installed definition
  runs the sampler at login and bakes in the state directory, socket path and
  recording flags resolved at install time, so a supervisor with neither herdr
  environment variable set still writes the history the panes read. Once
  installed, `--enable`, `--disable` and `--restore` defer to supervision.
  Restarts do not turn downtime into observation: every bucket the sampler was
  not running for remains a gap.
- `--since <WINDOW>` narrows `--once`, `--watch`, `--json` and `--week` to a
  requested reading window without changing what the sampler records. Each ring
  answers in its own bucket size and rounds the window up to at least one
  bucket. A window beyond retention draws only the recorded history, never
  invented quiet time; a window shorter than one bucket draws that one bucket.
  Both limits are explained on standard error.
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
- A second, coarser activity ring records the same samples as the fine ring in
  168 fixed one-hour buckets. It is recorded independently rather than derived:
  the default fine ring has aged out after four hours and cannot answer whether
  a workspace did anything yesterday. An hour nobody sampled remains a gap; an
  hour sampled without activity remains observed and quiet.

  The new `--week` verb renders those seven days as 28 six-hour columns while
  leaving the other report columns unchanged. `--json` adds a `"week"` array to
  each workspace and top-level `"week_bucket_seconds"` and `"week_columns"`
  fields. At the defaults, 8 workspaces × (240 fine + 168 week) buckets measure
  about 180 KB; the week ring is `168 / (240 + 168) ≈ 41%`, roughly 40%, of the
  history file.
- The `--once` and `--watch` pane views can now show a separate series for
  each recorded agent. Recording is opt-in through the bare `--agents` switch
  or the `per_agent_series` config key, and is off by default: an agent ring is
  as long as the fine ring and a workspace can retain up to four of them, so
  recording the rings multiplies what that workspace costs on disk and in every
  cycle's history rewrite. The cap is four agents per workspace, with the least
  recently seen evicted first.

  The sidebar badge keeps its single aggregate line. An agent first seen
  partway through the window has gaps before its first observation, so its line
  reads as absent-then-present rather than quiet-then-active.
- The pane now reports `blocked`, an estimate of how long an agent was observed
  blocked, and `--json` adds `blocked_seconds` alongside `watched_seconds` for
  every workspace. For each observed bucket, pulse multiplies the bucket
  duration by blocked samples divided by all samples, then rounds.
  `watched_seconds` says how much of the same `series` window the sampler
  actually observed, so the estimate cannot be mistaken for a measurement
  across the whole window. A gap contributes to neither figure: unobserved time
  is not time with no blocking in it.

- The pane views now put a `^` marker line under workspace and recorded-agent
  sparklines for columns in which pulse observed one or more state changes. Each
  row shows at most three marks: the columns with the highest observed
  transition counts, with ties going to the most recent column. That limit keeps
  the annotation readable instead of drawing under every eligible column; the
  single-line sidebar badge remains unchanged. A column nobody watched is never
  marked, and pulse does not infer a transition across a gap, because that would
  invent the moment the change happened. `--json` exposes the same evidence in
  `transitions` arrays beside workspace and agent `series`, and in
  `week_transitions` beside each workspace's `week`.

### Changed

- A gap used to say only that nobody was watching, not why. The pane now follows
  the history with a sampler line, and `--json` carries the same explanation:
  `disabled`, `terminated`, `failed` or `unknown`. `--disable` clears the enabled
  marker before signalling the sampler, so a signal received with the marker
  gone is the user's request (`disabled`); with the marker still set, it came
  from elsewhere (`terminated`). A panic or an error exit records itself as
  `failed`. A run that leaves no stop marker while the enabled marker says it was
  wanted reads as `unknown`, never dressed up as a tidy exit.
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
- A change to `bucket_seconds` used to discard the entire recorded history. A
  whole-multiple increase now folds each group of old buckets into one and keeps
  it, summing the counters because a bucket counts observations. A decrease
  still discards because a smaller bucket cannot be recovered from a larger one;
  an increase that is not a whole multiple still discards because the new
  boundaries would split observations that were never recorded separately. A
  folded group containing any unobserved time is itself unobserved rather than
  quiet.
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
