//! The sample store: bucketing, eviction, persistence and recovery.
//!
//! Owned by the historian. `model.rs` and `config.rs` are the contract and must
//! not be edited here.
//!
//! # What this module is responsible for
//!
//! Turning a stream of [`Sample`]s into a bounded, honest, on-disk record of
//! what each workspace's agents were doing, and handing it back as
//! [`WorkspaceActivity`] for the renderer.
//!
//! # The three properties that matter
//!
//! 1. **Bounded by construction.** The ring is `retention_buckets` long and is
//!    never grown. Tracked workspaces are capped at `max_workspaces`, evicting
//!    the least recently seen. The file size must have a ceiling that does not
//!    depend on uptime, and there is a test that proves it over many thousands
//!    of samples.
//!
//! 2. **Gaps are not quiet.** A bucket the sampler did not observe is `None`,
//!    never `Level(0)`. If the daemon was stopped for forty minutes, those
//!    buckets render as a gap glyph. Drawing zeros for "we were not watching" is
//!    a lie, and it is the single easiest way for this plugin to be confidently
//!    wrong.
//!
//! 3. **A half-written file must not poison the next run.** Persist by writing a
//!    temporary file in the same directory and renaming it over the target, so a
//!    reader either sees the whole previous file or the whole new one. On load,
//!    a corrupt or unreadable file is reported on stderr and treated as empty
//!    history — never as a hard failure that stops the sampler, and never
//!    silently.
//!
//! # Recording activity
//!
//! Each bucket counts observations rather than storing a pre-computed level, so
//! the definition of "activity" stays a rendering decision and the stored data
//! stays raw and re-interpretable.
//!
//! Both signals are needed and neither is sufficient:
//!
//! * **Occupancy** — how many of the bucket's samples saw a working agent.
//!   Alone, this under-reports nothing, but it cannot distinguish one long turn
//!   from a stuck process, which is what `transitions` is for.
//! * **Transitions** — how many times an agent's `state_change_seq` moved.
//!   Alone, this under-reports a long single turn, which produces one transition
//!   in ten minutes of hard work. It is also the only way to see an agent that
//!   went working -> idle -> working *between* two samples.

use crate::config::Config;
use crate::model::{Sample, WorkspaceActivity};
use crate::Result;

/// Bump when the on-disk shape changes incompatibly. A file whose version is
/// **higher** than this was written by a newer pulse: discard it with a message
/// rather than misreading it, because a misread history renders as confident
/// nonsense.
pub const FORMAT_VERSION: u32 = 1;

/// One bucket of observations for one workspace.
///
/// `samples == 0` means the sampler did not observe this bucket at all. Every
/// other field is meaningless in that case and must be ignored by the renderer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    /// Snapshots that landed in this bucket. Zero is a gap.
    pub samples: u16,
    /// Samples in which at least one agent was `working`.
    pub working: u16,
    /// Samples in which at least one agent was `blocked`.
    pub blocked: u16,
    /// Observed `state_change_seq` movements, summed across the workspace's
    /// agents. Catches transitions that happened between two samples.
    pub transitions: u16,
}

impl Bucket {
    /// Whether the sampler observed this bucket at all.
    pub fn observed(&self) -> bool {
        self.samples > 0
    }
}

/// Everything recorded for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceHistory {
    pub workspace_id: String,
    /// The workspace's label when it was last seen.
    ///
    /// This is the guard against workspace-id reuse. herdr's ids (`w15`, `wM`)
    /// are session-scoped, and nothing promises that the `w15` of tomorrow's
    /// session is the `w15` whose history we recorded today. When the label for
    /// an id changes, the recorded buckets belong to a different workspace and
    /// are dropped rather than attributed to the new one — a wrong attribution
    /// is invisible and unfalsifiable, an empty sparkline is neither.
    pub label: String,
    /// Ring of buckets, indexed by absolute bucket number modulo the ring
    /// length. Fixed length; never grown.
    pub buckets: Vec<Bucket>,
    /// Absolute bucket number of the newest bucket written, so a reader can tell
    /// which ring slots are current and which are stale leftovers from a
    /// previous lap.
    pub newest_bucket: u64,
    /// Aggregate state at the last observation.
    pub state: String,
    /// Unix seconds at which `state` was first observed to hold. `None` until a
    /// transition has actually been seen — we must not claim a duration we
    /// inferred from our own start time.
    pub state_since: Option<u64>,
    /// Unix seconds of the last observation, for least-recently-seen eviction.
    pub last_seen: u64,
    /// Highest `state_change_seq` seen per agent at the last observation, so the
    /// next sample can compute transitions. Keyed by pane id.
    pub agent_seqs: Vec<(String, u64)>,
}

/// The whole store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct History {
    pub version: u32,
    /// Bucket width the file was written with. If the live config disagrees, the
    /// recorded buckets mean something different from what we are about to
    /// record: discard rather than mixing two scales in one series.
    pub bucket_seconds: u64,
    pub workspaces: Vec<WorkspaceHistory>,
}

impl History {
    /// A store with nothing recorded, matching `config`.
    pub fn empty(config: &Config) -> Self {
        Self {
            version: FORMAT_VERSION,
            bucket_seconds: config.bucket_seconds,
            workspaces: Vec::new(),
        }
    }

    /// Folds one sample in.
    ///
    /// Must be idempotent with respect to bucket allocation: many samples land
    /// in one bucket, and the ring slot is chosen from `sample.taken_at`, never
    /// from a running counter. A sample older than the newest bucket already
    /// written is dropped rather than rewriting history — a clock that jumps
    /// backwards must not corrupt the series.
    pub fn record(&mut self, _sample: &Sample, _config: &Config) {
        todo!("historian")
    }

    /// The renderer's view: one entry per workspace, each with a series that
    /// ends at the bucket containing `as_of`.
    ///
    /// `columns` is the number of *output* columns; each aggregates
    /// `buckets_per_column` consecutive buckets. A column is `None` when **none**
    /// of its buckets were observed, and otherwise averages only the observed
    /// ones — a partially observed column is real data and must not be dropped.
    pub fn activity(
        &self,
        _as_of: u64,
        _columns: usize,
        _buckets_per_column: usize,
        _config: &Config,
    ) -> Vec<WorkspaceActivity> {
        todo!("historian")
    }

    /// Serialised size in bytes, for the boundedness test.
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

/// Loads the history from the state dir.
///
/// A missing file is an empty history and is not an error. A corrupt file, a
/// file from a future `FORMAT_VERSION`, or one whose `bucket_seconds` disagrees
/// with `config` is reported on stderr and replaced with an empty history: we
/// would rather start over loudly than render a series we cannot interpret.
pub fn load(_config: &Config) -> History {
    todo!("historian")
}

/// Persists the history, atomically.
///
/// Write to `history.json.tmp` in the same directory and `rename` it over
/// `history.json`, so a reader never sees a partial file and a crash mid-write
/// leaves the previous complete file intact. An unwritable state dir is reported
/// once and is not fatal: losing history is much better than stopping the
/// sampler.
pub fn save(_history: &History, _config: &Config) -> Result<()> {
    todo!("historian")
}

/// Deletes the recorded history. Backs the `--forget` action.
pub fn forget() -> Result<()> {
    todo!("historian")
}
