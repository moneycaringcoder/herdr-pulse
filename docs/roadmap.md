# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Everything below is measured against the one rule the plugin rests on: **a gap
column is not a quiet column.** Any feature that would make "we were not
watching" render as "nothing happened" is wrong no matter what else it buys.

## Presentation

### Adaptive sparkline width — blocked upstream

Eight columns is chosen because sidebar cells are narrow and herdr starts eliding
beyond that. Using the width actually available needs herdr to report it, and as
of herdr 0.8.0 / protocol 19 it does not: no method queries sidebar geometry, and
`WorkspaceInfo` — the object a badge belongs to — carries no width. The evidence,
including why the one derivable-looking number is not the answer, is in
[docs/herdr-protocol.md](herdr-protocol.md#what-the-protocol-does-not-expose-the-width-a-badge-has).

This stays parked rather than approximated. A sparkline that is subtly wrong at
the edges is worse than one that is honestly narrow, and every way of guessing the
width from what the protocol does expose is wrong on some layout. It becomes
possible the day a snapshot says how many cells a token may occupy in a
workspace's sidebar row, after the row's other tokens and herdr's eliding.

## Interfaces

### A versioned `--json` schema

`--json` already distinguishes gaps as `null` from observed-quiet as `0`, which is
the distinction the whole plugin exists to preserve. A schema version makes that
contract explicit for anything scripting against it.
