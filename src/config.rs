//! Configuration, plugin identity, and the state/config directories herdr hands
//! us. Owned by the integrator; the other modules read it, none of them change
//! it.

use std::path::PathBuf;
use std::time::Duration;

use crate::Result;

pub const PLUGIN_ID: &str = "moneycaringcoder.pulse";

/// How often the daemon takes a `session.snapshot`.
///
/// One snapshot of a live 10-workspace / 18-agent session measured 12.7 ms
/// median and 34 KB on the wire, so 5 s is a 0.25% duty cycle — cheap enough
/// that the finer occupancy resolution is worth having. Twelve samples land in
/// each 60 s bucket, so a bucket can distinguish activity to about 8%.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 5;
pub const MIN_INTERVAL_SECONDS: u64 = 1;
/// Bounded so the derived TTL can never exceed herdr's 24h ceiling. The
/// compile-time assertion below keeps the two in step.
pub const MAX_INTERVAL_SECONDS: u64 = 3_600;

const MAX_TTL_MS: u64 = 86_400_000;
const _: () = assert!(MAX_INTERVAL_SECONDS.saturating_mul(3_000) <= MAX_TTL_MS);

/// One minute per bucket.
///
/// Chosen against the question this plugin answers — "which of these did
/// anything in the last hour" — rather than against the sampling rate. A
/// minute is fine enough that a two-minute burst is visible as its own step and
/// coarse enough that an hour fits in a handful of columns.
pub const DEFAULT_BUCKET_SECONDS: u64 = 60;
pub const MIN_BUCKET_SECONDS: u64 = 10;
pub const MAX_BUCKET_SECONDS: u64 = 3_600;

/// Four hours of one-minute buckets. Covers "since lunch" without unbounded
/// growth: the ring is allocated at this length and never grows.
pub const DEFAULT_RETENTION_BUCKETS: usize = 240;
pub const MIN_RETENTION_BUCKETS: usize = 8;
pub const MAX_RETENTION_BUCKETS: usize = 10_000;

/// Sidebar cells are narrow. Eight columns of block elements plus a state glyph
/// is about as much as fits before herdr starts eliding.
pub const DEFAULT_BADGE_COLUMNS: usize = 8;
pub const MIN_BADGE_COLUMNS: usize = 1;
pub const MAX_BADGE_COLUMNS: usize = 64;

/// Minutes of history the badge's eight columns span. 64 minutes over 8 columns
/// is 8 minutes per column, which reads as "the last hour" while dividing
/// evenly.
pub const DEFAULT_BADGE_WINDOW_MINUTES: u64 = 64;

/// Hard ceiling on tracked workspaces, so the history file's size is bounded by
/// construction rather than by how many workspaces a user happens to open.
/// Least-recently-seen workspaces are evicted first.
pub const DEFAULT_MAX_WORKSPACES: usize = 64;
pub const MAX_MAX_WORKSPACES: usize = 512;

/// Upper bound on the badge's window.
///
/// A day is already far past the point where eight columns say anything useful,
/// and without *some* ceiling the field is unbounded: it is settable only from
/// the config file, so nothing on the command line ever exercised it.
pub const MAX_BADGE_WINDOW_MINUTES: u64 = 1_440;

/// The coarse ring: one hour per bucket, 168 of them, which is exactly a week.
///
/// Fixed rather than configurable, and deliberately so. The fine ring is tuned
/// by the user against the question "what happened this afternoon"; this one
/// answers "did this workspace do anything yesterday", which has one sensible
/// answer and no reason to vary. Two knobs would also mean two ways for a config
/// change to invalidate recorded history, and one is enough.
pub const WEEK_BUCKET_SECONDS: u64 = 3_600;
pub const WEEK_RETENTION_BUCKETS: usize = 168;

/// 28 columns of 6 hours each, which covers the 168 buckets exactly. Wider than
/// the badge because the week is only ever drawn in a pane, where there is room.
pub const WEEK_COLUMNS: usize = 28;
pub const WEEK_BUCKETS_PER_COLUMN: usize = 6;

/// Ceiling on `(retention_buckets + WEEK_RETENTION_BUCKETS) × max_workspaces`,
/// which is what actually determines the history file's size.
///
/// Both rings count. Every workspace carries the fine ring the user sizes and
/// the fixed 168-bucket week ring beside it, so a bound that names only the fine
/// one would say the week ring is outside it — and the next per-workspace array
/// somebody adds would be too.
///
/// Clamping the fields separately is not enough to keep the promise that storage
/// is "bounded by construction": their documented maxima multiply out to tens of
/// millions of buckets, and a measured
/// `{"max_workspaces": 4096, "retention_buckets": 10000}` produced a 103 MB file
/// that the daemon then rewrote and fsync'd every five seconds. Each bucket
/// serialises to roughly 55 bytes, so this ceiling corresponds to a few
/// megabytes — generous next to the `(240 + 168) × 64 = 26,112` buckets (~1.4 MB)
/// the defaults use, and small enough that the rewrite stays cheap.
pub const MAX_TOTAL_BUCKETS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Seconds between snapshots.
    pub interval: Duration,
    /// Seconds of wall clock each history bucket covers.
    pub bucket_seconds: u64,
    /// How many buckets are retained per workspace. Fixed-length ring.
    pub retention_buckets: usize,
    /// Sparkline columns in the sidebar badge.
    pub badge_columns: usize,
    /// Minutes of history those columns span.
    pub badge_window_minutes: u64,
    /// Ceiling on tracked workspaces.
    pub max_workspaces: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECONDS),
            bucket_seconds: DEFAULT_BUCKET_SECONDS,
            retention_buckets: DEFAULT_RETENTION_BUCKETS,
            badge_columns: DEFAULT_BADGE_COLUMNS,
            badge_window_minutes: DEFAULT_BADGE_WINDOW_MINUTES,
            max_workspaces: DEFAULT_MAX_WORKSPACES,
        }
    }
}

impl Config {
    /// TTL for a badge push: three refresh cycles, so one missed cycle does not
    /// blink the badge out, clamped to herdr's ceiling.
    pub fn ttl_ms(&self) -> u64 {
        self.interval
            .as_secs()
            .saturating_mul(3_000)
            .clamp(1, MAX_TTL_MS)
    }

    /// How many buckets one badge column aggregates. At least one, so a badge
    /// window shorter than a bucket still renders something.
    ///
    /// Capped so the badge can never ask for more history than the ring is able
    /// to hold. Without the cap, `--bucket-seconds 10` at the default 64-minute
    /// window asks for 384 buckets against a 240-bucket ring, and the three
    /// oldest columns are permanent gaps that no amount of uptime can fill — on a
    /// workspace that has in fact been observed without interruption. Gaps are
    /// supposed to mean "we were not watching", so inventing them from a
    /// configuration arithmetic mismatch undermines the one signal this plugin
    /// exists to give.
    pub fn buckets_per_badge_column(&self) -> usize {
        let window_seconds = self.badge_window_minutes.saturating_mul(60);
        let columns = self.badge_columns.max(1);
        let per_column = window_seconds / self.bucket_seconds.max(1) / columns as u64;
        let affordable = self.retention_buckets / columns;
        (per_column as usize).clamp(1, affordable.max(1))
    }

    /// Seconds of history the whole ring spans.
    pub fn retention_seconds(&self) -> u64 {
        self.bucket_seconds
            .saturating_mul(self.retention_buckets as u64)
    }

    /// Clamps every field into its documented range. Applied after both the
    /// config file and the command line, so neither source can produce a
    /// configuration the rest of the code has to defend against.
    fn clamp(&mut self) {
        self.interval = Duration::from_secs(
            self.interval
                .as_secs()
                .clamp(MIN_INTERVAL_SECONDS, MAX_INTERVAL_SECONDS),
        );
        self.bucket_seconds = self
            .bucket_seconds
            .clamp(MIN_BUCKET_SECONDS, MAX_BUCKET_SECONDS);
        self.retention_buckets = self
            .retention_buckets
            .clamp(MIN_RETENTION_BUCKETS, MAX_RETENTION_BUCKETS);
        self.badge_columns = self
            .badge_columns
            .clamp(MIN_BADGE_COLUMNS, MAX_BADGE_COLUMNS);
        self.badge_window_minutes = self.badge_window_minutes.clamp(1, MAX_BADGE_WINDOW_MINUTES);
        self.max_workspaces = self.max_workspaces.clamp(1, MAX_MAX_WORKSPACES);

        // The file's size is the product of the per-workspace bucket count and
        // the workspace cap, not either one alone, so clamping them separately
        // leaves "bounded by construction" untrue. Every workspace carries the
        // fine ring *and* the fixed week ring, so the week's 168 buckets are part
        // of the count being bounded — leaving them out would let the file exceed
        // the ceiling by 168 buckets per workspace, which is the whole ceiling
        // over at a few hundred workspaces.
        //
        // `retention_buckets` is the user's explicit statement about how far back
        // they want to see, so the workspace cap gives way instead — losing the
        // least recently seen workspace is a smaller loss than silently
        // shortening everyone's history.
        let per_workspace = self
            .retention_buckets
            .saturating_add(WEEK_RETENTION_BUCKETS);
        if per_workspace.saturating_mul(self.max_workspaces) > MAX_TOTAL_BUCKETS {
            let affordable = (MAX_TOTAL_BUCKETS / per_workspace.max(1)).max(1);
            // Loud, because a user who asked to track 400 workspaces and is
            // silently given 6 would have no way to discover it.
            eprintln!(
                "pulse: retention_buckets {} plus the {WEEK_RETENTION_BUCKETS}-bucket week ring, \
                 x max_workspaces {}, exceeds the {} bucket ceiling; \
                 tracking {} workspaces instead",
                self.retention_buckets, self.max_workspaces, MAX_TOTAL_BUCKETS, affordable
            );
            self.max_workspaces = affordable;
        }
    }
}

pub fn load() -> Result<Config> {
    load_with_args(&[])
}

/// Loads the config file, then applies command-line overrides.
pub fn load_with_args(args: &[String]) -> Result<Config> {
    let mut config = load_file();
    if let Some(raw) = value_arg(args, "--interval")? {
        config.interval = Duration::from_secs(parse_number(&raw, "--interval")?);
    }
    if let Some(raw) = value_arg(args, "--bucket-seconds")? {
        config.bucket_seconds = parse_number(&raw, "--bucket-seconds")?;
    }
    if let Some(raw) = value_arg(args, "--retention-buckets")? {
        config.retention_buckets = parse_number(&raw, "--retention-buckets")? as usize;
    }
    if let Some(raw) = value_arg(args, "--columns")? {
        config.badge_columns = parse_number(&raw, "--columns")? as usize;
    }
    config.clamp();
    Ok(config)
}

fn parse_number(raw: &str, name: &str) -> Result<u64> {
    raw.trim()
        .parse::<u64>()
        .map_err(|err| format!("{name} {raw}: {err}").into())
}

/// The on-disk form. Every field is optional so a partial file overrides only
/// what it names, and unknown keys are ignored so a newer file does not break an
/// older binary.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileConfig {
    interval_seconds: Option<u64>,
    bucket_seconds: Option<u64>,
    retention_buckets: Option<usize>,
    badge_columns: Option<usize>,
    badge_window_minutes: Option<u64>,
    max_workspaces: Option<usize>,
}

fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Reads the config file over the defaults. A missing file is the normal case;
/// an unreadable or malformed one is a warning and the defaults, never a hard
/// failure — a typo in a config file must not stop the badge from rendering.
fn load_file() -> Config {
    let path = config_file();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("pulse: ignoring {}: {err}", path.display());
            }
            return Config::default();
        }
    };
    let file: FileConfig = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("pulse: ignoring malformed {}: {err}", path.display());
            return Config::default();
        }
    };

    let mut config = Config::default();
    if let Some(value) = file.interval_seconds {
        config.interval = Duration::from_secs(value);
    }
    if let Some(value) = file.bucket_seconds {
        config.bucket_seconds = value;
    }
    if let Some(value) = file.retention_buckets {
        config.retention_buckets = value;
    }
    if let Some(value) = file.badge_columns {
        config.badge_columns = value;
    }
    if let Some(value) = file.badge_window_minutes {
        config.badge_window_minutes = value;
    }
    if let Some(value) = file.max_workspaces {
        config.max_workspaces = value;
    }
    config
}

/// Value of `--name <VALUE>` or `--name=<VALUE>`, last occurrence winning. A
/// missing or malformed value the user typed is a hard error, unlike a
/// malformed config file: they are looking right at it and silently ignoring it
/// would be worse.
///
/// `daemon::forwarded_args` recognises the same two spellings, so an argument
/// survives being handed to the detached child.
fn value_arg(args: &[String], name: &str) -> Result<Option<String>> {
    let flag = format!("{name}=");
    let mut found = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix(&flag) {
            found = Some(value.to_string());
        } else if arg == name {
            found = Some(rest.next().ok_or(format!("{name} needs a value"))?.clone());
        }
    }
    Ok(found)
}

pub fn plugin_id() -> String {
    non_empty_env("HERDR_PLUGIN_ID").unwrap_or_else(|| PLUGIN_ID.to_string())
}

/// Where the daemon's markers and the history file live:
/// `~/.local/state/herdr/plugins/<id>/`.
///
/// herdr injects `HERDR_PLUGIN_STATE_DIR` into the commands it spawns and is
/// authoritative when it does, but the fallback has to resolve to the *same*
/// directory. The README encourages running the binary by hand during
/// development, and a fallback that pointed somewhere else — a temp dir — would
/// give `--enable` from a plugin action and `--disable` from a shell two
/// different state dirs: the hand-run disable finds no pid file, silently does
/// nothing, and leaves a daemon running that the user has no way to stop.
pub fn state_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_STATE_HOME", ".local/state")
                .join("herdr")
                .join("plugins")
                .join(plugin_id())
        })
}

/// Where the config file lives: `~/.config/herdr/plugins/config/<id>/`. Same
/// split-brain rule as [`state_dir`] — a config read by hand must be the config
/// herdr reads.
pub fn config_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_CONFIG_HOME", ".config")
                .join("herdr")
                .join("plugins")
                .join("config")
                .join(plugin_id())
        })
}

/// An XDG base directory. The variable wins when it is set to an absolute path
/// — the spec says a relative one must be ignored — otherwise `$HOME/<relative>`.
///
/// The temp path is a last resort for a process with no home directory at all
/// (an empty-environment service manager). It is the wrong place for state, but
/// it is better than writing to the working directory, which for a herdr plugin
/// is somebody's repository.
fn xdg_dir(variable: &str, relative: &str) -> PathBuf {
    if let Some(base) = non_empty_env(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return base;
    }
    match non_empty_env("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(home) => home.join(relative),
        None => std::env::temp_dir().join("herdr-no-home"),
    }
}

/// Marker: a daemon is live right now.
pub fn pid_file() -> PathBuf {
    state_dir().join("sampler.pid")
}

/// Marker: the user asked for a daemon at some point. Survives restarts, and is
/// what `--restore` consults.
pub fn enabled_flag() -> PathBuf {
    state_dir().join("enabled")
}

/// Marker: why the last sampler run ended, written as it goes.
///
/// Separate from the pid and enabled markers because it answers a different
/// question from either. The pid says whether one is live now, the enabled flag
/// says whether the user wants one, and this says what happened to the last one
/// — which is the only way a gap in the history can carry a reason.
pub fn stop_marker() -> PathBuf {
    state_dir().join("sampler.stop")
}

/// The recorded activity history.
pub fn history_file() -> PathBuf {
    state_dir().join("history.json")
}

/// herdr injects empty strings for absent context, so empty means unset.
pub fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
