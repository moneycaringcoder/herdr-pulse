<!-- LOGO -->

# pulse

Per-workspace agent activity history for [herdr](https://github.com/herdr).

herdr's sidebar tells you what an agent is doing *right now*. It does not tell
you the shape of the last hour, so between two glances a wedged agent and a busy
one look exactly the same. pulse samples every agent's lifecycle state on an
interval, records a bounded history, and draws a sparkline next to each
workspace:

> **Which of these actually did anything in the last hour, and which have been
> idle since lunch?**

<!-- DIAGRAM: mechanism -->

## The one rule worth knowing

A blank column is **not** a quiet column.

| Glyph | Meaning |
|---|---|
| `▁▂▃▄▅▆▇█` | Observed activity, low to high |
| `·` | Observed, and nothing happened |
| *(blank)* | **Not observed** — the sampler was not running then |

"We were not watching" and "nothing happened" are different facts. pulse never
draws one as the other; a stopped sampler leaves a visible hole in the history
rather than a convincing stretch of calm.

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
| `badge_window_minutes` | `64` | ≥ 1 | Minutes of history those columns span. |
| `max_workspaces` | `64` | 1–4096 | Ceiling on tracked workspaces; least recently seen are evicted first. |

Command-line options override the file. Every value is clamped into its range,
so no configuration can produce output the renderer has to defend against.

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
- herdr's workspace ids are session-scoped and can be reused. When a workspace's
  label changes under an id, the recorded buckets are dropped rather than
  attributed to the new workspace — an empty sparkline is a visible loss, a wrong
  one is not.
- Changing `bucket_seconds` invalidates the recorded history, which is discarded
  with a message on the next run. Two bucket scales cannot share one series.

## License

MIT.
