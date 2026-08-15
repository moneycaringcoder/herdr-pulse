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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::model::{AgentState, Level, Sample, WorkspaceActivity, WorkspaceObservation};
use crate::Result;

/// Bump when the on-disk shape changes incompatibly. A file whose version is
/// **higher** than this was written by a newer pulse: discard it with a message
/// rather than misreading it, because a misread history renders as confident
/// nonsense.
pub const FORMAT_VERSION: u32 = 1;

/// The persisted store, and the temporary file it is renamed from. Both names
/// live here rather than in `config.rs` because [`forget`] has to remove the
/// pair: a `.tmp` left behind by a crashed write is recorded history too, and
/// leaving it after `--forget` would resurrect nothing but would still be a file
/// the user asked to be gone.
const HISTORY_FILE: &str = "history.json";
const TEMP_FILE: &str = "history.json.tmp";

/// How much of a [`Level`] occupancy is allowed to claim, leaving the rest for
/// churn.
///
/// Six of the eight steps come from "was anything working", so a workspace that
/// worked through the whole bucket still leaves two steps of headroom for the
/// transition count to separate one long uninterrupted turn from a dozen short
/// ones. Without that headroom both render as a full bar and the sparkline
/// cannot answer the question it exists for.
const OCCUPANCY_STEPS: u32 = 6;

/// How far behind the newest recorded bucket a sample may fall before we
/// conclude the recorded *anchor* is wrong rather than the sample.
///
/// Two buckets, because that is the largest honest disagreement: `taken_at` is
/// stamped before the snapshot round trip, so consecutive samples can cross a
/// bucket boundary in the wrong order by one bucket, and one more absorbs a slow
/// server. Anything further back is a clock that was stepped. See
/// [`WorkspaceHistory::observe`] for why erring towards re-anchoring is right.
const REANCHOR_AFTER_BUCKETS: u64 = 2;

/// Set once a save has failed, so a state dir that is unwritable for an hour
/// produces one line on stderr rather than one per sampling interval. Cleared by
/// a successful save so a problem that comes back is reported again.
static SAVE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

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

    /// This bucket's activity on the sparkline's scale.
    ///
    /// Only meaningful for an observed bucket; an unobserved one is a gap and
    /// never reaches here, because a gap has no level at all.
    ///
    /// Blocked deliberately contributes nothing. An agent waiting on a human is
    /// not doing work, and drawing a tall bar for a workspace that has been
    /// stuck for an hour would say the opposite of the truth. The blocked signal
    /// is carried by the state glyph and the badge tone, which is where a user
    /// can act on it.
    fn level(&self) -> u8 {
        if !self.observed() {
            return 0;
        }
        let samples = u32::from(self.samples);
        // A `working` count above `samples` can only come from a hand-edited or
        // corrupt file; clamp rather than letting it produce an over-full bar.
        let working = u32::from(self.working.min(self.samples));
        // Rounded rather than truncated: with twelve samples a bucket that was
        // busy for eleven of them should not read the same as one busy for ten.
        let occupancy = (working * OCCUPANCY_STEPS + samples / 2) / samples;
        // The exact transition count is not trustworthy enough to scale
        // linearly — it is a sum over agents of "this one moved at least once" —
        // so it buys presence, not magnitude.
        let churn = match self.transitions {
            0 => 0,
            1..=2 => 1,
            _ => 2,
        };
        (occupancy as u8).saturating_add(churn).min(Level::MAX)
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

impl WorkspaceHistory {
    /// A workspace with a ring but nothing in it, seeded from the observation
    /// that introduced it.
    ///
    /// `state_since` starts `None` on purpose. The state we are seeding with may
    /// have held for hours before this process started, and stamping it with
    /// "now" would report a five-second-old block on a workspace that has been
    /// waiting since lunch. We only know a duration once we have watched a
    /// change happen.
    fn new(observation: &WorkspaceObservation, taken_at: u64, config: &Config) -> Self {
        Self {
            workspace_id: observation.workspace_id.clone(),
            label: observation.label.clone(),
            buckets: vec![Bucket::default(); config.retention_buckets],
            newest_bucket: bucket_number(taken_at, config),
            state: observation.state().as_str().to_string(),
            state_since: None,
            last_seen: taken_at,
            agent_seqs: Vec::new(),
        }
    }

    /// The ring slot holding absolute bucket `number`, or `None` when that
    /// bucket is not in the ring's current lap.
    ///
    /// This is the guard that keeps a previous lap's data from being read as
    /// this one's. Every slot always holds *something*, and `number % len` is
    /// happy to hand back a slot written a full lap ago — which is a plausible
    /// looking bar drawn in a column the sampler never observed. Both ends
    /// matter: `number` above `newest_bucket` is the future (the ring holds the
    /// same slot's value from the previous lap), and `number` more than a lap
    /// below it has already been overwritten by newer data.
    fn slot(&self, number: u64) -> Option<usize> {
        let len = self.buckets.len() as u64;
        if len == 0 || number > self.newest_bucket {
            return None;
        }
        // `number + len` has to be checked. `newest_bucket` comes straight out of
        // the history file, and one near `u64::MAX` overflows the add — a panic
        // in a debug build today, and a bare SIGABRT the day anyone turns on
        // `overflow-checks` for the release profile, on the `pulse --once` path
        // that runs in somebody's sidebar. An overflow means the lap edge is past
        // `u64::MAX` and so past `newest_bucket`: the bucket is inside the lap.
        if number
            .checked_add(len)
            .is_some_and(|lap_edge| lap_edge <= self.newest_bucket)
        {
            return None;
        }
        Some((number % len) as usize)
    }

    /// The bucket for an absolute bucket number, or a gap if it is not in the
    /// ring's current lap.
    fn bucket(&self, number: u64) -> Bucket {
        self.slot(number)
            .map(|slot| self.buckets[slot])
            .unwrap_or_default()
    }

    /// Re-lays the ring at a new length, keeping each bucket at the slot its
    /// absolute number implies.
    ///
    /// Reached when a user changes `retention_buckets` between runs. Growing or
    /// truncating the vector in place would be far simpler and completely wrong:
    /// the ring is indexed modulo its length, so changing the length silently
    /// re-points every slot at a different minute. That is a whole sparkline of
    /// real-looking data describing the wrong times, which is exactly the class
    /// of failure this module is built to refuse.
    fn reshape(&mut self, len: usize) {
        if self.buckets.len() == len {
            return;
        }
        let mut fresh = vec![Bucket::default(); len];
        let keep = (self.buckets.len() as u64).min(len as u64);
        for back in 0..keep {
            let Some(number) = self.newest_bucket.checked_sub(back) else {
                break;
            };
            if let Some(slot) = self.slot(number) {
                fresh[(number % len as u64) as usize] = self.buckets[slot];
            }
        }
        self.buckets = fresh;
    }

    /// Moves the newest bucket forward, clearing every slot passed over.
    ///
    /// The clearing is the point. Those slots hold the previous lap's counts,
    /// and the minutes we skipped are minutes we did not observe — they have to
    /// come back as gaps, not as whatever was there four hours ago. At most one
    /// lap's worth of clearing is ever needed: a longer jump means the whole
    /// ring is stale, and every slot gets reset exactly once.
    fn advance(&mut self, number: u64) {
        let len = self.buckets.len() as u64;
        if len == 0 {
            self.newest_bucket = number;
            return;
        }
        let skipped = number - self.newest_bucket;
        for step in 0..skipped.min(len) {
            let stale = number - step;
            self.buckets[(stale % len) as usize] = Bucket::default();
        }
        self.newest_bucket = number;
    }

    /// Pulls the anchor *back* to `number`, discarding every bucket newer than
    /// it, and re-stamps the timestamps that were written alongside them.
    ///
    /// The counterpart to [`Self::advance`], and the only escape from a
    /// `newest_bucket` that a fast clock wrote into the future. Without it the
    /// drop rule in [`Self::observe`] discards every later sample until the wall
    /// clock climbs back past that bucket — for as long as the forward jump was,
    /// across daemon restarts and reboots, because `newest_bucket` is persisted.
    ///
    /// Discarding the newer buckets is not a loss: they were stamped by the same
    /// wrong clock and describe minutes that never happened. `last_seen` and
    /// `state_since` carry that clock too, and `WorkspaceActivity` measures
    /// freshness from them, so a future `last_seen` left in place would report
    /// "observed just now" forever beside a sparkline of pure gaps.
    fn rewind(&mut self, number: u64, taken_at: u64) {
        let len = self.buckets.len() as u64;
        if len > 0 {
            let ahead = self.newest_bucket - number;
            for step in 0..ahead.min(len) {
                let unreachable = self.newest_bucket - step;
                self.buckets[(unreachable % len) as usize] = Bucket::default();
            }
        }
        self.newest_bucket = number;
        self.last_seen = taken_at;
        if self.state_since.is_some_and(|since| since > taken_at) {
            // We no longer know when this state began: the only stamp we had for
            // it was a time that has not happened. "Unknown" is the honest
            // answer, and the same one a freshly tracked workspace gives.
            self.state_since = None;
        }
    }

    /// How many of this observation's agents have moved since the last one.
    ///
    /// The comparison is `!=`, not `>`. `state_change_seq` is session-global and
    /// a herdr restart begins a fresh sequence, so a seq that went *down* is
    /// still an agent that changed state — treating it as "no movement" would go
    /// quiet for exactly as long as it takes the new session to climb past the
    /// old numbers.
    ///
    /// An agent we have not seen before contributes nothing. A first sighting is
    /// not evidence of a transition, and counting it would give every workspace
    /// a spurious blip on the first sample after every daemon start — the one
    /// moment a user is most likely to be looking.
    fn count_transitions(&self, observation: &WorkspaceObservation) -> u16 {
        let mut transitions: u16 = 0;
        for agent in &observation.agents {
            let found = self
                .agent_seqs
                .binary_search_by(|(pane, _)| pane.as_str().cmp(agent.pane_id.as_str()));
            if let Ok(index) = found {
                if self.agent_seqs[index].1 != agent.state_change_seq {
                    transitions = transitions.saturating_add(1);
                }
            }
        }
        transitions
    }

    /// Folds one observation of this workspace in.
    ///
    /// Returns whether the anchor had to be pulled back to meet this sample, so
    /// the caller can report it once for the sample rather than once per
    /// workspace.
    fn observe(
        &mut self,
        observation: &WorkspaceObservation,
        taken_at: u64,
        config: &Config,
    ) -> bool {
        self.reshape(config.retention_buckets);

        let number = bucket_number(taken_at, config);
        let mut rewound = false;
        if number < self.newest_bucket {
            // A sample is by definition "now", so an anchor ahead of it means one
            // of the two clocks is wrong. Which one decides everything: dropping
            // the sample protects a bucket we have already finished reporting,
            // but if the *anchor* is the bogus value then dropping is a decision
            // to record nothing until the wall clock catches up with a time that
            // never happened.
            //
            // A bucket or two of slack absorbs the honest case: `taken_at` is
            // stamped before the round trip, so a sample can straddle a bucket
            // boundary and land marginally behind its predecessor. Beyond that
            // the clock has been stepped, and everything ahead of this sample was
            // stamped by the stepped clock — so re-anchor and drop it.
            //
            // The alternative of only re-anchoring when the sample is more than a
            // whole ring behind was rejected: it is the same bug with a shorter
            // fuse, leaving the store deaf for up to a full retention window (four
            // hours at the defaults) after an ordinary NTP correction.
            if self.newest_bucket - number <= REANCHOR_AFTER_BUCKETS {
                return false;
            }
            self.rewind(number, taken_at);
            rewound = true;
        }
        if number > self.newest_bucket {
            self.advance(number);
        }

        let transitions = self.count_transitions(observation);
        if let Some(slot) = self.slot(number) {
            let bucket = &mut self.buckets[slot];
            // Saturating throughout: a pathological interval/bucket-width pair
            // could in principle push past u16, and a wrapped counter reads as a
            // suddenly quiet workspace, which is a wrong answer nobody can see.
            bucket.samples = bucket.samples.saturating_add(1);
            if observation
                .agents
                .iter()
                .any(|a| a.state == AgentState::Working)
            {
                bucket.working = bucket.working.saturating_add(1);
            }
            if observation
                .agents
                .iter()
                .any(|a| a.state == AgentState::Blocked)
            {
                bucket.blocked = bucket.blocked.saturating_add(1);
            }
            bucket.transitions = bucket.transitions.saturating_add(transitions);
        }

        let state = observation.state();
        if self.state != state.as_str() {
            self.state = state.as_str().to_string();
            self.state_since = Some(taken_at);
        }

        // Only the agents present right now. Keeping seqs for agents that have
        // gone would be unbounded growth for a workspace that churns panes, and
        // the returning-agent case it would catch is indistinguishable from a
        // first sighting anyway.
        self.agent_seqs = observation
            .agents
            .iter()
            .map(|agent| (agent.pane_id.clone(), agent.state_change_seq))
            .collect();
        // Sorted so `count_transitions` can binary search and so the persisted
        // file is byte-identical for identical input. Highest seq wins a
        // duplicate pane id, which herdr should never produce, so that a
        // repeated id cannot manufacture a transition on the next sample.
        self.agent_seqs
            .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        self.agent_seqs.dedup_by(|a, b| a.0 == b.0);

        self.last_seen = self.last_seen.max(taken_at);
        rewound
    }

    /// The level for one output column, covering absolute buckets `first..=last`.
    ///
    /// `None` only when *nothing* in the range was observed. A column that is
    /// half gap and half data is data: it is real, and dropping it would punch a
    /// hole in the sparkline every time the daemon restarted mid-column.
    fn column(&self, first: u64, last: u64) -> Option<Level> {
        let len = self.buckets.len() as u64;
        if len == 0 {
            return None;
        }
        // Clamp the walk to the live lap — `[newest_bucket - len + 1,
        // newest_bucket]` — so a column wider than the ring still costs at most
        // one lap of iteration however far into the future `as_of` is.
        //
        // Both ends are anchored on `newest_bucket`, never on the column. An
        // earlier version measured a ring length back from `last`, which is the
        // *column's* newest bucket and can be well past `newest_bucket` whenever
        // the workspace stopped being reported before `as_of`. The walk then
        // started after the oldest live slots and never reached them, so a column
        // holding minutes of real, still-retained data read as a gap.
        // `--bucket-seconds 10 --columns 1` is enough to reach it.
        let first = first.max(self.newest_bucket.saturating_sub(len - 1));
        let last = last.min(self.newest_bucket);

        let mut total: u32 = 0;
        let mut observed: u32 = 0;
        for number in first..=last {
            let bucket = self.bucket(number);
            if !bucket.observed() {
                continue;
            }
            total += u32::from(bucket.level());
            observed += 1;
        }
        if observed == 0 {
            return None;
        }
        Some(Level::new(((total + observed / 2) / observed) as u8))
    }
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
    /// from a running counter. A sample slightly older than the newest bucket
    /// already written is dropped rather than rewriting history; one far enough
    /// behind that the recorded anchor cannot be believed re-anchors the store
    /// and says so, because a clock that steps forward and comes back must not
    /// leave the store permanently deaf.
    pub fn record(&mut self, sample: &Sample, config: &Config) {
        let mut rewound = 0usize;
        for observation in &sample.workspaces {
            if self.record_workspace(observation, sample.taken_at, config) {
                rewound += 1;
            }
        }
        // Once for the sample, not once per workspace: one clock step moves every
        // workspace at the same instant, and the next sample is already
        // re-anchored, so this is one line per event rather than a stream.
        // Discarding recorded history without saying so is how a user ends up
        // staring at a badge of gaps with a live pid and no hint of the cause.
        if rewound > 0 {
            eprintln!(
                "pulse: the recorded history was ahead of the clock; \
                 discarded the unreachable buckets for {rewound} workspace(s) and resumed recording"
            );
        }
        // After, not before: a workspace introduced by this sample is the most
        // recently seen thing in the store and must never be the one evicted.
        self.evict(config);
    }

    /// Returns whether folding this observation in had to re-anchor the
    /// workspace's ring.
    fn record_workspace(
        &mut self,
        observation: &WorkspaceObservation,
        taken_at: u64,
        config: &Config,
    ) -> bool {
        let existing = self
            .workspaces
            .iter()
            .position(|w| w.workspace_id == observation.workspace_id);
        let index = match existing {
            Some(index) => {
                if self.workspaces[index].label != observation.label {
                    // Same id, different workspace. herdr's ids are
                    // session-scoped and get reused, so the recorded buckets
                    // describe somebody else's afternoon. Start over: an empty
                    // sparkline is a fact the user can check, a borrowed one is
                    // not.
                    self.workspaces[index] = WorkspaceHistory::new(observation, taken_at, config);
                }
                index
            }
            None => {
                self.workspaces
                    .push(WorkspaceHistory::new(observation, taken_at, config));
                self.workspaces.len() - 1
            }
        };
        self.workspaces[index].observe(observation, taken_at, config)
    }

    /// Enforces the workspace cap, dropping the least recently seen first.
    ///
    /// This is half of the size bound — the ring bounds one workspace, this
    /// bounds how many rings exist — so it runs on every record and on every
    /// load, including when a config change lowers the cap under a file that was
    /// written with a higher one.
    fn evict(&mut self, config: &Config) {
        let cap = config.max_workspaces.max(1);
        while self.workspaces.len() > cap {
            let victim = self
                .workspaces
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    a.last_seen
                        .cmp(&b.last_seen)
                        // Ties broken by id so the same input always evicts the
                        // same workspace, whatever the map iteration order
                        // upstream happened to be.
                        .then_with(|| a.workspace_id.cmp(&b.workspace_id))
                })
                .map(|(index, _)| index);
            match victim {
                Some(index) => {
                    self.workspaces.remove(index);
                }
                None => break,
            }
        }
    }

    /// Brings a store read off disk in line with the live config, so nothing
    /// downstream has to defend against a file that was written by a differently
    /// configured run — or hand-edited.
    fn normalise(&mut self, config: &Config) {
        // A duplicate id would have `record` updating one entry while `activity`
        // reported both, so one of the two sparklines would quietly freeze.
        let mut seen: Vec<String> = Vec::new();
        self.workspaces.retain(|workspace| {
            if seen.iter().any(|id| id == &workspace.workspace_id) {
                return false;
            }
            seen.push(workspace.workspace_id.clone());
            true
        });
        for workspace in &mut self.workspaces {
            workspace.reshape(config.retention_buckets);
        }
        self.evict(config);
    }

    /// The renderer's view: one entry per workspace, each with a series that
    /// ends at the bucket containing `as_of`.
    ///
    /// `columns` is the number of *output* columns; each aggregates
    /// `buckets_per_column` consecutive buckets. A column is `None` when **none**
    /// of its buckets were observed, and otherwise averages only the observed
    /// ones — a partially observed column is real data and must not be dropped.
    ///
    /// Workspaces come back in the order they were first seen, which is the
    /// order they are stored in and is stable across a save and load. A
    /// workspace whose whole window is a gap is still reported: "this workspace
    /// exists and we have nothing recent for it" is the renderer's call to make,
    /// not the store's — which is what `last_seen` is for. `state` is always a
    /// past observation, `last_seen` says how old it is, and `state_for` is
    /// measured between the two rather than up to `as_of`.
    pub fn activity(
        &self,
        as_of: u64,
        columns: usize,
        buckets_per_column: usize,
        config: &Config,
    ) -> Vec<WorkspaceActivity> {
        let per_column = buckets_per_column.max(1) as u64;
        let newest = bucket_number(as_of, config);
        self.workspaces
            .iter()
            .map(|workspace| {
                let series = (0..columns)
                    .map(|column| {
                        // Oldest first: column 0 is the furthest back, and the
                        // last column is the one containing `as_of`.
                        let back = ((columns - 1 - column) as u64).saturating_mul(per_column);
                        // A column entirely before the epoch is not a column.
                        let last = newest.checked_sub(back)?;
                        let first = last.saturating_sub(per_column - 1);
                        workspace.column(first, last)
                    })
                    .collect();
                WorkspaceActivity {
                    workspace_id: workspace.workspace_id.clone(),
                    label: workspace.label.clone(),
                    series,
                    state: AgentState::parse(&workspace.state),
                    // Measured to the last observation, never to `as_of`. The
                    // difference is the whole point: an agent that was working
                    // when we last looked five hours ago has been *observed*
                    // working for however long we watched it, not for five hours.
                    // Measuring to now would print "working 5h00" in the same row
                    // as a sparkline of pure gaps, and `--json` would hand a
                    // downstream tool the same claim.
                    //
                    // Saturating: a `state_since` ahead of `last_seen` can only
                    // come from a corrupt file, and a wrapped subtraction would
                    // report a duration of several hundred billion years with
                    // complete confidence.
                    state_for: workspace
                        .state_since
                        .map(|since| workspace.last_seen.saturating_sub(since)),
                    // The stamp that makes `state` falsifiable. A recorded
                    // workspace always carries the time of the observation that
                    // created it, so a zero here can only come from a hand-edited
                    // file: report it as never observed rather than as a 1970
                    // sighting, which would read as ancient-but-real.
                    last_seen: Some(workspace.last_seen).filter(|seen| *seen > 0),
                    agent_count: workspace.agent_seqs.len(),
                }
            })
            .collect()
    }

    /// Serialised size in bytes, for the boundedness test.
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

/// The absolute bucket number containing a Unix timestamp.
///
/// Derived from wall clock rather than from a counter of samples, so two samples
/// in the same minute land in the same bucket no matter how many were dropped
/// between them, and so a restart resumes into the right bucket instead of
/// starting a new one.
fn bucket_number(at: u64, config: &Config) -> u64 {
    at / config.bucket_seconds.max(1)
}

/// Loads the history from the state dir.
///
/// A missing file is an empty history and is not an error. A corrupt file, a
/// file from a future `FORMAT_VERSION`, or one whose `bucket_seconds` disagrees
/// with `config` is reported on stderr and replaced with an empty history: we
/// would rather start over loudly than render a series we cannot interpret.
pub fn load(config: &Config) -> History {
    load_from(&state_dir(), config)
}

/// [`load`], from a named directory.
///
/// The directory is a parameter so the tests can exercise every recovery path
/// without touching `HERDR_PLUGIN_STATE_DIR` — a process-global that two tests
/// running in parallel threads would fight over, which is how a suite starts
/// passing or failing depending on the machine.
pub fn load_from(dir: &Path, config: &Config) -> History {
    let path = dir.join(HISTORY_FILE);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // The normal first run. Not a problem, so not a message.
            return History::empty(config);
        }
        Err(err) => {
            eprintln!(
                "pulse: starting with an empty history, cannot read {}: {err}",
                path.display()
            );
            return History::empty(config);
        }
    };

    let mut history: History = match serde_json::from_slice(&raw) {
        Ok(history) => history,
        Err(err) => {
            eprintln!(
                "pulse: starting with an empty history, {} is corrupt: {err}",
                path.display()
            );
            return History::empty(config);
        }
    };

    if history.version > FORMAT_VERSION {
        eprintln!(
            "pulse: starting with an empty history, {} is format {} and this pulse understands {FORMAT_VERSION}",
            path.display(),
            history.version
        );
        return History::empty(config);
    }
    if history.version < FORMAT_VERSION {
        // There is no migration path yet. When one is written it belongs here;
        // until then, reading an older shape as if it were the current one is
        // the misinterpretation the version field exists to prevent.
        eprintln!(
            "pulse: starting with an empty history, {} is format {} and cannot be upgraded",
            path.display(),
            history.version
        );
        return History::empty(config);
    }
    if history.bucket_seconds != config.bucket_seconds {
        eprintln!(
            "pulse: starting with an empty history, {} has {}s buckets and this run uses {}s",
            path.display(),
            history.bucket_seconds,
            config.bucket_seconds
        );
        return History::empty(config);
    }

    history.normalise(config);
    history
}

/// Persists the history, atomically.
///
/// Write to `history.json.tmp` in the same directory and `rename` it over
/// `history.json`, so a reader never sees a partial file and a crash mid-write
/// leaves the previous complete file intact. An unwritable state dir is reported
/// once and is not fatal: losing history is much better than stopping the
/// sampler.
///
/// Hence the `Ok` on failure. The caller is the sampling loop, and the obvious
/// thing for it to write is `save(&history, &config)?` — so a full disk must not
/// be an error here, or a full disk stops the badge from ever updating again.
pub fn save(history: &History, _config: &Config) -> Result<()> {
    match save_to(&state_dir(), history) {
        Ok(()) => {
            SAVE_FAILURE_REPORTED.store(false, Ordering::Relaxed);
        }
        Err(err) => {
            if !SAVE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "pulse: not recording history, cannot write {}: {err}",
                    state_dir().join(HISTORY_FILE).display()
                );
            }
        }
    }
    Ok(())
}

/// [`save`], to a named directory, reporting the failure it hit rather than
/// swallowing it. The tests drive this one; `save` is the daemon's
/// never-fatal wrapper around it.
pub fn save_to(dir: &Path, history: &History) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(HISTORY_FILE);
    let temp = dir.join(TEMP_FILE);
    let encoded = serde_json::to_vec(history).map_err(std::io::Error::other)?;

    // The temp file has to be in the same directory: `rename` is only atomic
    // within a filesystem, and a temp dir elsewhere would silently degrade to a
    // copy that can be interrupted halfway.
    if let Err(err) = write_all_synced(&temp, &encoded) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&temp, &target) {
        // Leaving the temp behind would have the next crash-recovery reader find
        // two files and no way to tell which is real.
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// Writes and flushes to the platter before returning, so the rename cannot
/// commit a name that points at unwritten bytes.
fn write_all_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Deletes the recorded history. Backs the `--forget` action.
pub fn forget() -> Result<()> {
    forget_in(&state_dir())
}

/// [`forget`], in a named directory.
///
/// Unlike [`save`], a failure here is returned: the user typed `--forget` and is
/// looking at the terminal, and "your history is gone" when it is not would be a
/// lie about the one thing they asked for.
pub fn forget_in(dir: &Path) -> Result<()> {
    for name in [HISTORY_FILE, TEMP_FILE] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Already gone is the requested end state.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("cannot delete {}: {err}", path.display()).into()),
        }
    }
    Ok(())
}

fn state_dir() -> PathBuf {
    crate::config::state_dir()
}
