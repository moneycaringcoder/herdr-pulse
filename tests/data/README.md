# Test fixtures

## `snapshot-live.json`

A real `session.snapshot` response, captured from a running herdr 0.8.0 server
(protocol 19) with 10 workspaces, 18 agents and 18 panes.

Every key, nesting level and value *type* is exactly as the server sent it. Only
identifying string values were replaced — workspace labels, agent names,
repository names and paths, terminal titles, and the agent-session UUID. Nothing
was added, removed or restructured.

In particular, *which* entries carry an optional field is preserved: 15 of the 18
`agents[]` entries have a `name` and 3 do not, exactly as captured, and one
carries a `display_agent`. That distribution is the point — a fixture where every
entry looks the same cannot catch a client that assumes a field is always there.

The point of capturing rather than hand-writing this is that a fake built from
assumptions tests only the assumptions. The structural detail that matters most
here is that the arrays live under `result.snapshot`, not under `result`; a
client that reads them one level too high returns no data at all, which is
indistinguishable from an idle session.

Observed `agent_status` values in this capture: `done`, `idle`, `working`.

## `snapshot-blocked.json`

**Derived, not captured.** `snapshot-live.json` with `agent_status` changed to
`blocked` on pane `w15:p4`, on that pane's `agents[]` entry, and on the
aggregated status of workspace `w15`.

No agent entered the `blocked` state during the capture window, so this fixture
exists to exercise that path. It is structurally real; those three fields are
not. `blocked` is in the server's own `AgentStatus` schema, so the value is
legitimate — but do not cite this file as evidence of what a live blocked agent
looks like in full.
