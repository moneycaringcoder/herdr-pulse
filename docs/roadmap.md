# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Everything below is measured against the one rule the plugin rests on: **a gap
column is not a quiet column.** Any feature that would make "we were not
watching" render as "nothing happened" is wrong no matter what else it buys.

## Keeping history that is currently lost

### A second, coarser ring

The 240-bucket ring covers four hours, chosen for "since lunch". A separate
hourly ring covering a week would answer "did this workspace do anything
yesterday" without touching the fine-grained one or its fixed size ceiling.

## More useful numbers

### Blocked time, not just activity

The sparkline shows that an agent was active. The number people actually act on
is how long agents sat *blocked* waiting for input, because that is the time a
human could have given back. The sampler already records the state; this is a
question of surfacing it.

### Per-agent series

Three agents in one workspace currently aggregate into one line. That is right
for the sidebar, where there is room for one series. It is not right for the
`--watch` pane, which has room to separate them.

### Mark transitions

A column that shows activity does not show that the agent went `blocked` halfway
through it. Annotating transitions in the wider pane views puts the moment
something changed next to the shape of the work around it.

## Operating it

### Say why the sampler stopped

A gap correctly shows that nothing was observed. It does not say whether the
sampler was disabled deliberately, killed, or crashed. The distinction matters to
anyone trying to trust the history.

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
