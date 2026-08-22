# Versioned JSON schema

## The version

Every `pulse --json` document carries `"schema_version": 1` at its top level.
The implementation owns that number as `pulse::render::JSON_SCHEMA_VERSION`; the
field lets a consumer identify the contract before interpreting anything else in
the document.

Key order is not part of the contract. The document is written from an ordered
map, so keys come out alphabetically and `as_of` happens to appear first — read
`schema_version` by name, never by position.

## Guarantees

### `null` is not `0`

**`null` in any bucket array is a bucket nobody observed; `0` is a bucket that
was observed and had nothing in it. They are different facts.** A consumer that
coerces `null` to `0` — through a serde default, JavaScript's `?? 0`, Rust's
`unwrap_or_default`, or an equivalent convenience — turns “we were not
watching” into “nothing happened”. That is the failure pulse exists to prevent.

This rule applies to every workspace's `series`, `week`, `transitions` and
`week_transitions`, and to every nested agent's `series` and `transitions`.
Transition arrays carry the same evidence as activity arrays: a positive integer
is an observed count, `0` means pulse watched the bucket and saw no transition,
and `null` means pulse did not watch it. Consumers must not bridge a `null` to
infer a transition.

## What the version promises

The version promises the document's shape: which keys exist, where they are
nested, each value's type and unit, and what each value means. It does not freeze
the values; those change every time the sampler runs.

It also does not promise the contents or positions of `workspaces`. That array
follows the order in which workspaces were first seen, so consumers must not
attach meaning to an array index or sort it by assumption.

## Schema version 1 fields

Types below are JSON types. `integer` means a non-negative JSON integer. “Unix
seconds” means seconds since the Unix epoch; plain “seconds” means a duration.
Array paths use `[]` for one element.

| Key path | Type | Unit | Meaning |
|---|---|---|---|
| `as_of` | integer | Unix seconds | The instant the document describes. Ages and freshness are evaluated relative to it. |
| `bucket_seconds` | integer | seconds per bucket | The wall-clock width of one bucket in the fine history ring. |
| `buckets_per_column` | integer | buckets per column | The number of fine-ring buckets aggregated into each output column. |
| `columns` | integer | columns | The number of columns in each fine activity and transition array. |
| `level_max` | integer | activity level | The inclusive upper bound for an observed activity level; version 1 uses `8`. |
| `sampler` | object | — | The sampler's current liveness and the separately recorded outcome of its last run. |
| `sampler.running` | boolean | — | Whether a sampler is live now. This is read independently of `sampler.stopped`. |
| `sampler.stopped` | object or null | — | The recorded outcome of the last sampler run, or `null` when there is no stop record. It is not the inverse of `running`. |
| `sampler.stopped.at` | integer or null | Unix seconds | When the run ended, or `null` when no process was able to record a time. |
| `sampler.stopped.detail` | string or null | — | One line of failure detail when one was recorded, otherwise `null`. |
| `sampler.stopped.reason` | string | — | Why the last run stopped: `disabled`, `terminated`, `failed` or `unknown`. |
| `schema_version` | integer | — | The version of this document-shape contract; version 1 is `1`. |
| `seconds_per_column` | integer | seconds per column | The fine column width: `bucket_seconds × buckets_per_column`. |
| `session` | object | — | The live herdr session pulse can establish while producing the document. Its fields are nullable when no live session can be established. |
| `session.began` | integer or null | Unix seconds | When the live session's socket started listening, or `null` when no live session can be established. |
| `session.fingerprint` | string or null | — | The live session's opaque socket fingerprint, or `null` when no live session can be established. |
| `staleness_tolerance_seconds` | integer | seconds | The maximum observation age that still permits `state_is_current`; it is derived from three sampling intervals. |
| `week_bucket_seconds` | integer | seconds per bucket | The wall-clock width of one bucket in the coarse week ring; version 1 uses `3600`. |
| `week_buckets_per_column` | integer | buckets per column | The number of week-ring buckets aggregated into each week column; version 1 uses `6`. |
| `week_columns` | integer | columns | The number of columns in each week activity and transition array; version 1 uses `28`. |
| `week_seconds_per_column` | integer | seconds per column | The week column width: `week_bucket_seconds × week_buckets_per_column`; version 1 uses `21600`. |
| `workspaces` | array of objects | — | The recorded workspace-session rows. Distinct herdr sessions remain distinct rows because their histories are not comparable. Rows follow first-seen order. |
| `workspaces[].agent_count` | integer | agents | The number of agents in the workspace at its last observation. |
| `workspaces[].agents` | array of objects | — | Per-agent histories retained for this workspace. The array is empty unless per-agent recording produced rings. |
| `workspaces[].agents[].blocked_seconds` | integer | seconds | Estimated time this agent was observed blocked over its `series` window. Read it with the agent's `watched_seconds`. |
| `workspaces[].agents[].last_seen` | integer | Unix seconds | When this agent was last observed. |
| `workspaces[].agents[].observed_ago_seconds` | integer | seconds | The agent's observation age: `as_of - last_seen`, saturating at zero if the recorded time is ahead. |
| `workspaces[].agents[].pane_id` | string | — | The herdr pane id that identifies this agent within its session. |
| `workspaces[].agents[].program` | string or null | — | The agent program herdr reported, such as `claude` or `opencode`, or `null` when none was reported. |
| `workspaces[].agents[].series` | array of integer or null | activity level per column | This agent's fine history, oldest first. Integers are observed levels from `0` through `level_max`; `null` is an unobserved bucket, including time before the agent appeared. |
| `workspaces[].agents[].state` | string | — | The agent's lifecycle state at its last observation: `blocked`, `working`, `idle`, `done` or `unknown`. |
| `workspaces[].agents[].state_is_current` | boolean | — | Whether this agent's `last_seen` age is within `staleness_tolerance_seconds`. |
| `workspaces[].agents[].transitions` | array of integer or null | transitions per column | Observed state-change counts aligned oldest-first with the agent's `series`; `0` is watched with no change and `null` is not watched. |
| `workspaces[].agents[].watched_seconds` | integer | seconds | How much of this agent's `series` window pulse actually observed. A gap adds nothing. |
| `workspaces[].blocked_seconds` | integer | seconds | Estimated blocked time over observed buckets in this workspace's fine `series` window. Read it with `watched_seconds`, not the row's wall-clock width. |
| `workspaces[].label` | string | — | The user's label for the workspace. |
| `workspaces[].last_seen` | integer or null | Unix seconds | The most recent observation of this workspace, or `null` when it has never been observed. |
| `workspaces[].observed_ago_seconds` | integer or null | seconds | The workspace's observation age relative to `as_of`, or `null` when `last_seen` is `null`. |
| `workspaces[].series` | array of integer or null | activity level per column | The workspace's fine activity history, oldest first. Integers are observed levels from `0` through `level_max`; `null` is an unobserved bucket. |
| `workspaces[].session` | object | — | Provenance for the herdr session that recorded this row. Unknown provenance is represented by nullable fields, not by joining the row to another session. |
| `workspaces[].session.began` | integer or null | Unix seconds | When that session's socket started listening, or `null` when it could not be established. |
| `workspaces[].session.fingerprint` | string or null | — | The opaque socket fingerprint of the session that recorded the row, or `null` when it could not be established. |
| `workspaces[].session.is_current` | boolean or null | — | Whether this row belongs to the live session. It is `null`, not `false`, when there is no live session to compare against. |
| `workspaces[].sparkline` | string | — | The rendered form of `series`, included so a report can show what the user saw without a screenshot. |
| `workspaces[].state` | string | — | The workspace's aggregate lifecycle state at its last observation: `blocked`, `working`, `idle`, `done` or `unknown`. Blocked outranks working in the aggregation. |
| `workspaces[].state_for_seconds` | integer or null | seconds | How long `state` had held when pulse last observed the workspace, measured to `last_seen` and never extrapolated to `as_of`; `null` when the duration is unknown. |
| `workspaces[].state_is_current` | boolean | — | Whether this workspace has a `last_seen` age within `staleness_tolerance_seconds`; it is `false` when the workspace has never been observed. |
| `workspaces[].transitions` | array of integer or null | transitions per column | Observed state-change counts aligned oldest-first with `series`; `0` is watched with no change and `null` is not watched. |
| `workspaces[].watched_seconds` | integer | seconds | How much of the fine `series` window pulse actually observed. A gap adds nothing. |
| `workspaces[].week` | array of integer or null | activity level per column | The workspace's coarse week history, oldest first. It uses the same `null`-versus-`0` rule as `series`. |
| `workspaces[].week_blocked_seconds` | integer | seconds | Estimated blocked time over observed buckets in this workspace's `week` window. Read it with `week_watched_seconds`. |
| `workspaces[].week_transitions` | array of integer or null | transitions per column | Observed state-change counts aligned oldest-first with `week`; `0` is watched with no change and `null` is not watched. |
| `workspaces[].week_watched_seconds` | integer | seconds | How much of the `week` window pulse actually observed. A gap adds nothing. |
| `workspaces[].workspace_id` | string | — | herdr's session-scoped workspace id for this row. |

## When the version moves

Bump `pulse::render::JSON_SCHEMA_VERSION` in the same pull request for any change
a consumer can notice by reading the shape: removing or renaming a key, moving a
key between objects, changing a value's type or unit, or changing what an
existing key means.

Adding a new key beside the existing keys does not move the version. A consumer
that reads only the keys it knows remains unaffected, but the addition still
earns a changelog entry. `tests/render.rs` pins the complete set of key paths, so
a shape move without a version bump fails CI.

## Reading the document safely

Keep the two bucket facts distinct before doing any arithmetic. For example,
JavaScript can turn the series into an explicit observed/unobserved sum type and
use watched time as the blocked estimate's denominator:

```js
const buckets = row.series.map((level) =>
  level === null
    ? { observed: false }
    : { observed: true, level } // level may be 0
);

const blockedShare = row.watched_seconds === 0
  ? null
  : row.blocked_seconds / row.watched_seconds;
```

The first branch preserves a gap rather than hiding it behind `level ?? 0`. The
ratio uses the seconds pulse actually watched. Dividing `blocked_seconds` by
`columns * seconds_per_column` would count every gap as observed and unblocked,
which states the opposite of the evidence.
