<img src="docs/img/logo.svg" alt="" width="96" align="right">

# pulse

Per-workspace agent activity history for [herdr](https://github.com/herdr).

herdr's sidebar tells you what an agent is doing *right now*. It does not tell
you the shape of the last hour, so between two glances a wedged agent and a busy
one look exactly the same. pulse samples every agent's lifecycle state on an
interval, records a bounded history, and draws a sparkline next to each
workspace:

> **Which of these actually did anything in the last hour, and which have been
> idle since lunch?**

<p align="center">
  <img src="docs/img/mechanism.svg" alt="One session.snapshot every five seconds is folded into a per-workspace ring of one-minute buckets, which is drawn as a sidebar badge and an activity pane." width="620">
</p>

## The one rule worth knowing

A gap column is **not** a quiet column.

| Glyph | Meaning |
|---|---|
| `▁▂▃▄▅▆▇█` | Observed activity, low to high |
| `·` | Observed, and nothing happened |
| `╌` | **Not observed** — the sampler was not running then |

"We were not watching" and "nothing happened" are different facts. pulse never
draws one as the other; a stopped sampler leaves a visible hole in the history
rather than a convincing stretch of calm.

The gap is a printing character rather than a blank for a reason that is not
cosmetic: herdr trims whitespace off a badge token's value and deletes a token
whose value is entirely whitespace. With a space, the *newest* columns of a
sparkline were silently stripped, so the series stopped lining up with the
present — and a workspace nobody had been watching lost its badge altogether,
which is the one moment the gap most needed showing.

## What it looks like

A real `pulse --once`, captured from a running session. Only the workspace labels
have been renamed; every glyph, column and number is as the plugin printed it.

```
pulse — 19 workspaces — 23:32:18 UTC

workspace           activity                            state          for  seen  agents
web-api             [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·▃·····]  ? unknown      35m  3s    0
research            [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·······]  ? unknown      ?    3s    0
planner             [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·······]  ? unknown      32m  3s    0
media-fix           [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·······]  - idle         ?    3s    2
budget-fix          [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·······]  = done         ?    3s    2
shear               [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▆▆▇▆▆▂·]  = done         11m  3s    1
redact              [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▆▆▆▆▆▆▆]  > working      ?    3s    3
collide             [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▆▆▆▆▆▆▆]  > working      ?    3s    5
standup             [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▆▆▇▇▆▆▅]  ! blocked      14m  3s    4
pulse               [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌▆▆▆▆▆▆▆]  > working      ?    3s    5
app                 [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·······]  ? unknown      ?    3s    0
repo                [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌····]  ? unknown      ?    3s    0
label-probe         [╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌·╌╌╌]  ? was unknown  ?    24m   0

legend  ▁▂▃▄▅▆▇█ busier  |  · observed, nothing happened  |  ╌ not observed
        for = how long the state had held when last seen  |  seen = how long ago that was
        "was X" is the last observation and nothing has been seen since — not a claim about now
        one column = 7m  |  whole row = 3h44
```

Four things worth reading off that:

- **`standup` is `! blocked`** — one of its four agents is waiting on a human
  while the others finish. That workspace will not make progress until somebody
  looks at it, and it is the only row where that is true.
- **`shear` is winding down.** `▆▆▇▆▆▂·` is a busy stretch that tailed off; the
  state caught up a moment later and says `= done`.
- **The leading `╌` are honest.** The sampler had been running for about half an
  hour when this was taken, so most of the four-hour row is time nobody watched —
  drawn as gaps, not as calm.
- **`label-probe` says `was unknown`, `seen 24m`.** Nothing has been observed
  there for 24 minutes, so the row refuses to make a present-tense claim, and its
  newest columns are gaps rather than quiet.

In the sidebar each workspace gets the compact form — eight columns over the last
64 minutes, plus the state glyph:

```
standup   ╌╌▆▇▇▆▆▅!
collide   ╌╌▆▆▆▆▆▆>
budget    ╌╌······=
```

## Install

```sh
herdr plugin install moneycaringcoder/herdr-pulse
```

For local development, from a checkout:

```sh
cargo build --release      # herdr plugin link does NOT build for you
herdr plugin link .
```

`herdr plugin link` only registers the directory. If you skip the build step
there is no `target/release/pulse` for herdr to run, and every verb fails.

## Sidebar setup

herdr only renders a plugin's custom tokens if your `config.toml` names them.
pulse contributes three, one per severity, because herdr renders a token's value
as flat text and cannot colour by content:

| Token | Lit when |
|---|---|
| `$pulse_blocked` | At least one agent is waiting on a human |
| `$pulse_working` | At least one agent is working |
| `$pulse_quiet` | Nothing working, nothing blocked |

Run this and it is done for you:

```sh
pulse --setup
```

It splices the entries into your existing `[ui.sidebar.spaces]` rows, takes a
backup at `config.toml.pulse-backup` first, reloads herdr's config, and restores
the backup automatically if that reload fails. Running it twice is a no-op.
`pulse --setup-rollback` undoes it.

To do it by hand instead, add the three entries inside a row of your
`[ui.sidebar.spaces]` rows array:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch",
    { token = "$pulse_blocked", fg = "#FF8080" },
    { token = "$pulse_working", fg = "#8CD98C" },
    { token = "$pulse_quiet",   fg = "#8A8A8A" },
  ],
]
```

The entries have to land **inside** a row, not between two rows. A bare table
dropped beside a row is still valid TOML, so herdr accepts the file and then
renders nothing at all.

## Usage

Start the sampler once; it survives herdr restarts.

```sh
pulse --enable
pulse --once
```

### Reporting

| Verb | What it does |
|---|---|
| `--once` | Print a one-shot activity report and exit. The default when no verb is given. |
| `--json` | Print the recorded history as JSON and exit. Gaps are `null`, observed-and-quiet is `0`. |
| `--watch` | Live activity view, refreshing on an interval. Exits with a message if no sampler is running. |

### Reading the pane

`--once` and `--watch` print one row per workspace:

| Column | Meaning |
|---|---|
| `workspace` | The workspace's label. |
| `activity` | The sparkline, oldest column on the left, newest on the right. |
| `state` | The state the workspace was in **at the last observation**. |
| `for` | How long that state had held when we last looked. |
| `seen` | How long ago that observation was. |
| `agents` | Agents in the workspace at that observation. |

Every state in the pane is a past observation. While the sampler is running the
last observation is seconds old, so reading it as the present is fair and the row
says `> working`. Once the last observation is older than three sampling
intervals — because the sampler stopped, or the machine slept — the row switches
to the past tense:

| Row reads | Means |
|---|---|
| `> working` | An agent is working, as of a moment ago. |
| `> was working` | An agent was working when we last looked, `seen` ago. Nothing is claimed about now. |

The sparkline says the same thing in glyphs: a stretch of `╌` is time nobody
watched. The words and the sparkline are never allowed to disagree.

`--json` carries the same distinction, since a consumer cannot read a tense:
each workspace has `last_seen`, `observed_ago_seconds` and `state_is_current`,
and the document has the `staleness_tolerance_seconds` those were judged
against.

### Sampler

| Verb | What it does |
|---|---|
| `--enable` | Start the background sampler, detached from herdr. |
| `--disable` | Stop it and clear every badge this plugin set. |
| `--toggle` | Stop it if running, otherwise start it. |
| `--restore` | Restart it only if it was enabled. herdr's startup hook; silent otherwise. |
| `--daemon` | Run the sampler in the foreground. Internal; `--enable` uses it. |

### History

| Verb | What it does |
|---|---|
| `--forget` | Delete the recorded history and start over. |

### Sidebar setup

| Verb | What it does |
|---|---|
| `--setup` | Add pulse's tokens to herdr's `config.toml` and reload. |
| `--setup-rollback` | Restore the `config.toml` backup taken by `--setup`. |

### Other

| Option | What it does |
|---|---|
| `--interval <SECS>` | Seconds between snapshots (default 5). |
| `--bucket-seconds <SECS>` | Wall clock per history bucket (default 60). |
| `--retention-buckets <N>` | Buckets retained per workspace (default 240). |
| `--columns <N>` | Sparkline columns in the badge (default 8). |
| `--version` | Print version and exit. |
| `--help` | Show help. |

Options may appear before or after the verb: `pulse --interval 10 --once` and
`pulse --once --interval 10` are the same command.

## Configuration

Settings live at:

```
~/.config/herdr/plugins/config/moneycaringcoder.pulse/config.json
```

Every key is optional — a partial file overrides only what it names, and unknown
keys are ignored. A missing file is normal. A malformed one is reported on
stderr and the defaults are used, because a typo should not stop your badges
from rendering.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `interval_seconds` | `5` | 1–3600 | Seconds between snapshots of herdr's session. |
| `bucket_seconds` | `60` | 10–3600 | Wall clock each history bucket covers. |
| `retention_buckets` | `240` | 8–10000 | Buckets kept per workspace. Fixed-length ring; never grows. |
| `badge_columns` | `8` | 1–64 | Sparkline columns in the sidebar badge. |
| `badge_window_minutes` | `64` | 1–1440 | Minutes of history those columns span. |
| `max_workspaces` | `64` | 1–512 | Ceiling on tracked workspaces; least recently seen are evicted first. |

Command-line options override the file. Every value is clamped into its range,
so no configuration can produce output the renderer has to defend against.

Two of those ranges interact, and pulse says so rather than quietly obeying:

- **`retention_buckets × max_workspaces` is capped at 65,536 buckets**, which is
  what actually determines the history file's size — clamping each field on its
  own would leave "bounded by construction" untrue, since their maxima multiply
  out to a file of tens of megabytes rewritten every few seconds. When the pair
  exceeds the cap the workspace ceiling gives way, and the reason is printed on
  stderr. `retention_buckets` wins because it is your explicit statement about
  how far back you want to see.
- **The badge never asks for more history than the ring holds.** `--bucket-seconds
  10` with the default 64-minute window would want 384 buckets from a 240-bucket
  ring, leaving three columns permanently gapped on a workspace that was in fact
  watched the whole time. The window is shortened to fit instead, because a gap
  has to mean "nobody was watching" and not "the arithmetic did not line up".

```json
{
  "interval_seconds": 5,
  "bucket_seconds": 60,
  "retention_buckets": 240,
  "badge_columns": 8,
  "badge_window_minutes": 64,
  "max_workspaces": 64
}
```

State — the recorded history and the sampler's markers — lives separately, at
`~/.local/state/herdr/plugins/moneycaringcoder.pulse/`.

## Why these numbers

**60-second buckets.** The bucket size is chosen against the question, not
against the sampling rate. A minute is fine enough that a two-minute burst
appears as its own step in the sparkline, and coarse enough that an hour of
history fits into a handful of columns. At the default 5-second interval, twelve
samples land in each bucket, so a bucket can distinguish activity to about 8%.

**240 buckets = 4 hours.** Enough to cover "since lunch", which is the longest
span anyone asks this question about. The ring is allocated at this length and
never grows, so the history file's size has a ceiling that does not depend on
uptime.

**8 badge columns over 64 minutes = 8 minutes per column.** Sidebar cells are
narrow; eight columns plus a state glyph is about as much as fits before herdr
starts eliding. 64 minutes divides evenly by 8 and still reads as "the last
hour". The `--once` and `--watch` panes are not so constrained and draw the full
retention across 32 wider columns instead.

**5-second sampling.** One snapshot of a live 10-workspace, 18-agent session
measured 12.7 ms and 34 KB on the wire, so a 5-second interval is a 0.25% duty
cycle — cheap enough that the finer resolution is worth having.

## Costs and caveats

- The sampler is a detached background process. It keeps running after herdr
  exits until you run `pulse --disable`.
- Badges are pushed with a TTL of three refresh cycles, so they self-clear if the
  sampler is killed rather than lingering as stale claims.
- herdr's workspace ids are session-scoped and can be reused. History is keyed on
  the workspace's checkout path where herdr reports one, so a series survives an
  id it shares with somebody else tomorrow. A workspace with no worktree has no
  such key: for those, a label that changes under an id is treated as a different
  workspace and the recorded buckets are dropped rather than attributed to it —
  an empty sparkline is a visible loss, a wrong one is not.
- Changing `bucket_seconds` invalidates the recorded history, which is discarded
  with a message on the next run. Two bucket scales cannot share one series.

## License

MIT.
