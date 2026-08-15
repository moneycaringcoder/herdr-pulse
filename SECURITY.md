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
- **Any subprocess beyond the two declared ones** — the daemon re-execing itself,
  and `herdr server reload-config` during setup. pulse never invokes git.
- **Any outbound network traffic.** pulse makes no network calls at all, so any
  is a bug by definition. There is no telemetry and no update check.
- **Leaking session contents.** The history file records workspace labels,
  workspace ids and agent lifecycle states. It should never record command lines,
  terminal contents, file paths, or repository contents — if you find something
  sensitive in it, that is a bug.
- **A path that lets a crafted socket response cause a crash, a hang, or an
  unbounded write.** The history file is bounded by construction; anything that
  defeats that bound is a denial-of-service issue.

## What is out of scope

- The `--setup` action deliberately edits `~/.config/herdr/config.toml`. It takes
  a backup first, refuses to clobber an existing backup, and restores the
  original if herdr rejects the result. That it writes to that one file, with
  consent, is the feature — not a vulnerability. A failure of the backup or
  rollback logic *is* in scope.
- Bugs in herdr itself belong at
  [herdr](https://github.com/SuperCodeAgents/herdr-terminal).

## Supported versions

The latest release is supported. Given the size of the project, fixes go into a
new release rather than being backported.
