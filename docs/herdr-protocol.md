# herdr socket protocol notes (verified against herdr 0.8.0, protocol 19)

Working notes for this plugin's socket client. Everything here was verified
against a live herdr 0.8.0 server and its bundled schema (`herdr api schema`),
not inferred from documentation. Where a claim was checked by reading a value
back out of the server rather than by trusting an `ok`, it says so.

## Transport

`HERDR_SOCKET_PATH` is injected into every command herdr spawns. Fall back to
`$XDG_CONFIG_HOME/herdr/herdr.sock` only for hand invocation. Treat an
empty-string environment variable as unset — herdr injects empty strings for
absent context rather than omitting the variable.

Framing is **newline-delimited JSON**. Not length-prefixed. There is no
`jsonrpc` field.

```
request : {"id":"<string>","method":"<name>","params":{...}}\n
success : {"id":"<string>","result":{"type":"<snake_case>",...}}\n
failure : {"id":"<string>","error":{"code":"<string>","message":"<string>"}}\n
```

- `id` must be a **string**.
- `params` is **mandatory and must be an object**. Sending `null` is rejected:
  `invalid request: invalid type: null, expected struct EmptyParams`.
- Every mutation returns `{"type":"ok"}`.

### The socket is one request per connection

After a single response the server sends EOF and closes. Every call must be able
to reconnect and retry once. This is the hot path, not an edge case, and the
same retry is what carries a client across a `herdr update --handoff`.

### Error envelopes, captured verbatim

```
{"id":"e1","error":{"code":"workspace_not_found","message":"workspace nope not found"}}
{"id":"e2","error":{"code":"invalid_metadata_token","message":"invalid metadata token key: $bad name"}}
{"id":"e3","error":{"code":"invalid_metadata_ttl","message":"metadata ttl_ms must be at least 1"}}
{"id":"","error":{"code":"invalid_request","message":"invalid request: ..."}}
```

Note the last one: a request the server could not even parse comes back with
**`"id":""`**, not the id that was sent. A client that matches responses by id
must not depend on the echo.

## `session.snapshot` — params `{}`

This is the only read this plugin makes. One call per interval covers the whole
session; there is no per-workspace call to make.

The payload is `{"type":"session_snapshot","snapshot":{...}}` and the arrays live
**one level below `result`, under `snapshot`**:

```
snapshot.version    "0.8.0"        snapshot.protocol  19
snapshot.workspaces[]  workspace_id, number, label, focused, pane_count,
                       tab_count, active_tab_id, agent_status, tokens?, worktree?
snapshot.agents[]      pane_id, workspace_id, tab_id, terminal_id, agent,
                       agent_session, agent_status, state_change_seq, revision,
                       cwd, foreground_cwd, focused, terminal_title, tokens
snapshot.panes[]       the same shape, plus `scroll`
snapshot.tabs[] snapshot.layouts[]
snapshot.focused_workspace_id / focused_tab_id / focused_pane_id
```

Reading the arrays off `result` instead of `result.snapshot` yields no data at
all, which is indistinguishable from an idle session. An absent `snapshot`
object must therefore be a loud error, never a fallback to empty.

### There is no session identity in here

Checked against the schema of a live 0.8.0 server, not inferred: `SessionSnapshot`
carries `version`, `protocol`, the four arrays and the three `focused_*` ids, and
nothing that distinguishes this run of the server from the last one. There is no
session id, no server start time, and no boot counter. The `session_start_source`
and `started_unix_ms` fields that turn up elsewhere in the schema belong to
*agent* sessions and to command results, not to the herdr session.

The read-only surface has no substitute either. Every `server.*` method —
`server.stop`, `server.live_handoff`, `server.reload_config`,
`server.agent_manifests`, `server.reload_agent_manifests` — is an action, and this
plugin observes and never acts.

So pulse takes the session's identity from the thing it is talking to: the socket
at `HERDR_SOCKET_PATH`. Its device and inode identify it among every socket on
the machine, and its creation time is the moment the server started listening,
which is the closest thing to "when this session began" that can be read without
having watched it start. Verified on a live server: the bound path is a socket,
and `(dev, ino, ctime)` are stable across repeated calls while it keeps running.

What that proves, and what it does not, is in **Still unverified** below. The
error it can make is to report two sessions where there was one, which splits a
history; the error it cannot make is to report one where there were two, which
would join two incomparable series.

### Two corrections to the notes this plugin inherited

Both confirmed against a live snapshot:

1. **`agents[]` entries carry the full pane shape**, not a reduced
   `{pane_id, tab_id, workspace_id, agent_session}`. `agents` is essentially
   `panes` filtered to those running a recognised agent.
2. **`name` is present but optional**, and it is the *user's* label for the
   agent, not the program. In the captured snapshot 15 of 18 entries carry one
   and 3 do not; one entry also carries a `display_agent`. `agent` is the
   program (`claude`, `opencode`) and is always present, so it is the field to
   rely on. `agent_session` is an object (`{agent, kind, source, value}`), not a
   string, so it is no use as a display name either.

   An earlier version of this document asserted there was no `name` field at
   all, which was simply wrong — the mistake came from reading one entry rather
   than the whole array. It is recorded here because it is the exact shape of
   error this project keeps finding: a confident claim about a payload, derived
   from too small a sample, that nothing downstream can contradict.

`workspaces[].worktree` also carries a `repo_name` field that the inherited
notes do not mention.

### `agent_status`

From the server's own schema, `success_response/$defs/AgentStatus`, the complete
enum is:

```
blocked | done | idle | unknown | working
```

The *write* side (`pane.report_agent`, `$defs/PaneAgentState`) accepts only four
— plugins cannot report `done`. This plugin only ever reads.

`workspaces[].agent_status` is herdr's own aggregation over the workspace's
agents. Observed live: a workspace with three `working` agents and one `idle`
reports `working`. This plugin computes its own aggregate instead, because it
ranks `blocked` above `working` — see `model::AgentState::rank`.

### `state_change_seq` is session-global

This is the field that makes the plugin possible, and the easiest one to
misread.

`state_change_seq` is a **single monotonic counter shared by the whole session**,
stamped onto an agent at the moment it last changed state. It is not per-agent,
and it does not count that agent's transitions.

Verified by sampling a live session every ten seconds: between two samples one
agent went `working -> idle` and was stamped 799, while a different agent went
`idle -> working` and was stamped 798. Both values came from one sequence.

Two consequences:

- An agent whose seq **differs** between two samples transitioned at least once,
  *even when its status string is unchanged*. This is the only way to observe an
  agent that went `working -> idle -> working` entirely inside one sampling
  interval — which is exactly the busy agent this plugin must distinguish from a
  wedged one.
- The **size** of the delta means nothing about that agent, because the counter
  also advanced for every other agent that moved. Treat a change as "at least
  one transition" and no more.

### Cost

One snapshot of a live session with 10 workspaces and 18 agents: **12.7 ms
median** (11.8 min, 15.2 max over 20 calls), **34 KB** on the wire. At the
default 5 s interval that is a 0.25% duty cycle, and it does not grow with the
number of workspaces the way an N+1 enumeration would.

## `workspace.report_metadata` — the badge

Required: `workspace_id`, `source`, `tokens`. Tokens-only.

```json
{"id":"pulse:7","method":"workspace.report_metadata","params":{
  "workspace_id":"w1D","source":"moneycaringcoder.pulse",
  "tokens":{"pulse_working":"·▃▆█ ▶"},"ttl_ms":15000}}
```

- `tokens` is a **merge patch**. Omitted names are untouched, `null` deletes.
  Max 16 keys per report; herdr stores at most 32 per target. Names must match
  `^[A-Za-z0-9_-]{1,32}$` — **no `$` on the wire**. The `$` prefix exists only in
  herdr's `config.toml` row syntax.
- `ttl_ms` is 1..86_400_000 and is what makes the badge self-heal when the daemon
  is killed. Derive it as ~3× the refresh interval so one missed cycle does not
  blink the badge out.
- `source` is the plugin id, namespacing ownership so we never clobber another
  plugin's tokens.
- `seq` is optional and costs a tracked "sequenced source" slot per target. Omit
  it — we are a single-writer daemon.

### Batching works, and the inherited notes were unsure

The notes this plugin inherited list token batching as an open question, and
assume one token per call. It is answered. Verified **by reading the tokens back
out of a subsequent `session.snapshot`**, not by trusting the `ok`:

| tried | result |
|---|---|
| set two tokens in one call | both present on readback |
| clear three tokens in one call, one never set | all absent on readback, no error |
| mix a set and a clear in one call | accepted, both effects applied |
| `ttl_ms` present alongside a `null` clear | **accepted**, not rejected |

So the badge push and the `--disable` sweep are **one round trip per
workspace**, not one per token. `ttl_ms` is still omitted on a clear-only call,
since there is nothing for it to apply to.

Readback also confirmed that Unicode block elements survive intact: `·▃▆█` came
back byte-identical, so the sparkline can use the full ramp.

### Token values are trimmed, and an all-whitespace value is a delete

Undocumented, and it cost this plugin a real bug. Verified by readback:

| sent | read back |
|---|---|
| `"   ab"` | `"ab"` — leading whitespace trimmed |
| `"ab   "` | `"ab"` — trailing whitespace trimmed |
| `"a   b"` | `"a   b"` — interior whitespace survives |
| `"\u{a0}\u{a0}ab"` | `"ab"` — the trim is Unicode-aware, so NBSP is no escape |
| `"     "` | *token absent* — an all-whitespace value **deletes** the token |

The consequence for anything drawing a fixed-width sparkline is severe, because
the failure is entirely silent. With a space as the "no data" glyph:

- trailing gaps — the **newest** columns — are stripped, so the sparkline stops
  being aligned to the present and older activity reads as current;
- leading gaps are stripped, so badge widths vary down the sidebar;
- a series that is *entirely* gaps renders as an empty string, herdr deletes the
  token, and the badge disappears at exactly the moment the record is least
  trustworthy.

So the gap glyph is a printing character (`╌`), not a blank, and
`tests/render.rs` pins that as a correctness property rather than a style
choice. The whole render suite passed with a space, because every assertion
referred to the glyph symbolically and moved along with the bug.

### Colour by token name

herdr renders a token's value as flat text and cannot colour by content. Severity
is encoded in the **token name**: light exactly one, clear the others.

```
pulse_blocked   pulse_working   pulse_quiet
```

Each gets its own `fg` in the user's config. Track which name is currently lit
per workspace so a flip clears the previous name first — otherwise the merge
patch leaves two badges lit at once.

Unlike a plugin that only warns, **all three names are worth configuring here**,
including the quiet one: a quiet workspace whose sparkline shows it was busy ten
minutes ago is the most useful thing this plugin says. The badge is cleared only
for a workspace with no recorded history at all.

## Nothing renders until the user edits config.toml

herdr's default sidebar rows name none of our tokens. `pulse --setup` splices
them in with a backup; the README also ships the snippet. Rows reload live via
`herdr server reload-config` — no restart needed.

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch",
    { token = "$pulse_blocked", fg = "#FF8080" },
    { token = "$pulse_working", fg = "#8CD98C" },
    { token = "$pulse_quiet",   fg = "#8A8A8A" }],
]
```

## Plugin execution environment

Commands are argv arrays run with **no shell**, cwd = plugin root, and a minimal
`PATH`. Plugins run on the **server** host. `herdr plugin link .` does **not**
run `[[build]]`; `herdr plugin install` does. Logs are in-server only
(`herdr plugin log list`), with no log file on disk.

State lives in `HERDR_PLUGIN_STATE_DIR`. Both the pid marker and the enabled
marker are needed — one answers "is a daemon live right now", the other answers
"did the user ever ask for one" — and both writes are best-effort, since an
unwritable state dir must not fail the user's action.

## What the protocol does not expose: the width a badge has

Checked, not assumed, against `herdr api schema --json` from a live 0.8.0 server
(protocol 19) and a live `session.snapshot`. **Nothing in the protocol says how
many columns a sidebar badge has to work with**, so the badge's eight columns are
a fixed choice rather than a measurement.

The evidence, in the order it rules the possibilities out:

- All 105 request methods were enumerated from the schema's `request.oneOf[]`.
  None queries sidebar or UI geometry: the closest are `pane.layout` and
  `pane.list`, which describe terminal panes.
- `WorkspaceInfo` — the object the badge belongs to — carries `workspace_id`,
  `number`, `label`, `focused`, `pane_count`, `tab_count`, `active_tab_id`,
  `agent_status`, `tokens` and `worktree`. There is no width, and no cell budget.
- The only widths in the whole schema are `PaneLayoutRect { x, y, width, height }`
  in uint16 cells, and `PaneScrollInfo.viewport_rows`, which is rows. Both
  describe terminal panes. A badge is not a pane.

There is a tempting derivation, and it is wrong. The observed live session
reports `layouts[].area.x == 28`, which on that machine is exactly the sidebar's
width. But `area.x` is where the *pane area* begins, and it equals the sidebar
width only when the sidebar is visible, on the left, and the only chrome to its
left. The protocol promises none of those, so the number is a coincidence that
happens to be right here and would be silently wrong elsewhere.

Even a true sidebar width would not answer the question. The row a badge lands in
is composed in the user's `config.toml` — `[ui.sidebar.spaces] rows` — where our
token shares a line with whatever else the user listed, and herdr decides at
render time how much each of those takes and where to elide. The observed config
sets no width at all: the 28 is herdr's own layout, computed from data the plugin
cannot see.

What would unblock it: a field on `WorkspaceInfo`, or a top level of
`session.snapshot`, giving the cells a token may occupy in that workspace's row
**after** the row's other tokens and herdr's eliding. Until that exists, widening
the sparkline means guessing, and a sparkline that is subtly wrong at the edges is
worse than one that is honestly narrow.

## Still unverified

Honest list of what this plugin assumes rather than checked:

- **Workspace id stability across a server restart.** herdr's ids (`w1D`, `wM`)
  are session-scoped and nothing observed here says whether a fresh server
  reissues them to different workspaces, so the store does not rely on the
  answer. It keys history on `workspaces[].worktree.checkout_path`, which names
  the same checkout in every session, and falls back to the id *and* the label
  for a workspace herdr reports no worktree for — dropping that workspace's
  buckets when the label under an id changes. Whether ids turn out to be durable
  or recycled, no series is attributed on the strength of an id alone.
- **Whether two workspaces can share one checkout path.** Nothing observed says
  they cannot, and the three worktrees in the capture are distinct, so a path
  seen twice in one snapshot is treated as no identity at all rather than as a
  reason to merge two workspaces' histories.
- **Whether a restarted server always re-binds a different socket.** A Unix
  socket has to be unlinked and re-bound to be listened on again, so a fresh
  server should always land on a new inode, and `(dev, ino, ctime)` should
  therefore differ. Not watched happening: the live server observed here has been
  bound since it started. Inode numbers *are* reused once a file is gone, which
  is why the creation time to the nanosecond is part of the fingerprint. If both
  ever collided, two sessions would look like one — the only direction of error
  that matters, and the reason it is worth stating out loud.
- **What a live handoff does to the socket.** `herdr update --handoff` replaces
  the process without ending the session. If it re-binds the socket, pulse will
  read that as a new session and split the history, which costs a seam that was
  not really there. That is the safe direction: a split says "these two stretches
  may not be comparable", which is true either way.
- **`blocked` in a real snapshot.** The status appears in the server's schema and
  in the write-side enum, but no agent in the observed session entered it during
  the capture window, so the fixture carrying it is structurally real with that
  one field edited, and is labelled as such.
- **How a stop is requested.** The lifecycle contract says "request stop" without
  naming a mechanism; `SIGTERM` is the assumption, matching the signal-thread
  language.
