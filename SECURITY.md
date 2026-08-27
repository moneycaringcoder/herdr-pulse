# Security policy

## Reporting a vulnerability

Please report security issues privately, through GitHub's
[private vulnerability reporting](https://github.com/moneycaringcoder/herdr-pulse/security/advisories/new)
rather than as a public issue.

You can expect an acknowledgement within a few days. Since this is a
single-maintainer project, please don't read silence as dismissal — follow up if
you have heard nothing after a week.

If you would rather not use GitHub's reporting flow, open a public issue saying
only that you have found a security problem and would like a private channel,
with no details, and one will be arranged.

## What counts as a security issue here

pulse reads a local herdr socket and writes badge tokens. It records what it
observes to a file in its own state directory. The things worth reporting
urgently:

- **Any write outside the plugin's state directory**, and in particular any write
  to a user's repository. pulse runs unattended on machines full of in-flight,
  uncommitted agent work, and it has no business touching any of it.
- **Any subprocess beyond the three declared classes** — the daemon re-execing
  itself, `herdr server reload-config` during setup, and the explicitly selected
  systemd/launchd supervisor commands. pulse never invokes git.
- **Any outbound network traffic.** pulse makes no network calls at all, so any
  is a bug by definition. There is no telemetry and no update check.
- **Leaking uncollected session contents.** `history.json` deliberately records
  workspace ids and labels, absolute checkout paths, socket-session
  fingerprints/start times, lifecycle state/timing/counters, pane ids and
  sequence counters, plus agent program names when per-agent recording is on.
  Named-session directory names reversibly encode the absolute Herdr socket
  pathname. `sampler.stop` may contain a bounded failure detail, including source
  or filesystem paths. Pulse must not persist command lines, terminal contents,
  repository contents, credentials, cwd/foreground-cwd, agent session ids or
  names, tokens, titles, unrelated environment values, or unknown snapshot
  fields.
- **Crash-persistent state is included in the same contract.**
  `history.json.tmp` may contain the complete history payload;
  `sampler.pid.tmp` and `default.socket.tmp` contain the corresponding PID/path;
  `default.socket.lock`, `sampler.owner.lock`, and `sampler.control.lock` are
  empty files carrying kernel lock state. Successful operations rename/remove
  their temporary files, and `--forget` removes the history temporary file.
- **A path that lets a crafted socket response cause a crash, a hang, or an
  unbounded write.** The history file is bounded by construction; anything that
  defeats that bound is a denial-of-service issue.

## Deliberate writes and security boundaries

- The `--setup` action deliberately edits the selected Herdr `config.toml`. Its
  owner-only backup and transaction metadata sit beside that file. Atomic
  replacement, validation restore, and rollback refusal are the feature; a
  failure that truncates the file, clobbers recovery state, or overwrites later
  user edits is in scope.
- Runtime state is private to its owner: exact plugin-owned directories are mode
  `0700`, files are `0600`, permissive legacy modes are tightened, and final
  symlinks are refused/replaced rather than followed. A write through a state
  symlink or permission migration outside the plugin-owned root is in scope.
- Bugs in Herdr itself belong at [herdr](https://github.com/herdrdev/herdr).

## Supported versions

The latest release is supported. Given the size of the project, fixes go into a
new release rather than being backported.
