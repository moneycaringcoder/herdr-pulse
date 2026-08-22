# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Everything below is measured against the one rule the plugin rests on: **a gap
column is not a quiet column.** Any feature that would make "we were not
watching" render as "nothing happened" is wrong no matter what else it buys.

## Operating it

### Optional supervision

The sampler is a detached background process that keeps running after herdr exits
until `pulse --disable`. It does not survive a reboot. Optional systemd and
launchd units would make "always watching" true across restarts, for people who
want that.

## Presentation

### Adaptive sparkline width

Eight columns is chosen because sidebar cells are narrow and herdr starts eliding
beyond that. If herdr ever reports the width actually available, the series could
use it rather than assuming the tightest case.

## Interfaces

### A versioned `--json` schema

`--json` already distinguishes gaps as `null` from observed-quiet as `0`, which is
the distinction the whole plugin exists to preserve. A schema version makes that
contract explicit for anything scripting against it.

### `--since` in the pane views

The panes draw the full retention. Narrowing to a window is a small addition and
the obvious next question once someone is looking at four hours of history.
