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
//! # Which workspace a series belongs to
//!
//! A series is only worth keeping if we can say whose it is. herdr's
//! `workspace_id` cannot answer that on its own: it is session-scoped and a
//! fresh server reissues the same `w15` to whatever workspace it likes. The
//! store therefore keys on the workspace's checkout path when herdr reports one,
//! which means the same thing in every session and lets a series survive a
//! reused id instead of being thrown away.
//!
//! A workspace with no worktree has no durable key, and for those the id and the
//! label together are the only evidence there is: a label that changes under an
//! id is treated as a different workspace and its buckets are dropped. Both
//! cases obey one rule — where identity cannot be established, drop rather than
//! guess. A recovered series must be provably the same workspace, never probably
//! one, because a sparkline drawn under the wrong name is wrong in a way nobody
//! can see.
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
use crate::model::{
    AgentState, Level, Sample, SessionMark, WorkspaceActivity, WorkspaceObservation,
};
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
    /// The session-scoped id herdr used for this workspace at the last
    /// observation. Unique *within a session*, because it is what a badge is
    /// pushed to: two entries of one session sharing an id would push two
    /// different sparklines to one workspace and the last write would win.
    ///
    /// Entries from different sessions may hold the same id, because that is
    /// what a session-scoped id means. Only the live session's entries are badge
    /// targets; an id from a session that has ended may belong to somebody else
    /// now.
    pub workspace_id: String,
    /// The workspace's label when it was last seen.
    pub label: String,
    /// The checkout path herdr reported for this workspace, and the durable
    /// identity the series is keyed on when there is one.
    ///
    /// A path outlives the session that observed it, so a workspace that comes
    /// back under a reused id is still recognisably itself and its buckets are
    /// kept. `None` means herdr reported no worktree, and there is no durable
    /// identity to key on — for those the id and the label together are all the
    /// evidence there is, and a label that changes under an id means the buckets
    /// belong to somebody else and are dropped. See [`History::locate`].
    ///
    /// Defaulted on load. An entry with no recorded path is adopted, and stamped
    /// with one, by the first observation that carries a path **in that entry's
    /// own session** — a workspace herdr has only just started reporting a
    /// worktree for. A file written before this field existed carries no session
    /// either, so it is not adopted by a named session at all: it keeps its
    /// buckets as one unattributable watch beside the attributed series that
    /// follows it. See [`Self::session`].
    #[serde(default)]
    pub checkout_path: Option<String>,
    /// Fingerprint of the herdr session that recorded this ring, or `None` for a
    /// ring whose session could not be established.
    ///
    /// This is the other half of identity, and it is not the same question as
    /// [`Self::checkout_path`]. The path answers *which workspace* these buckets
    /// belong to; the session answers *which watch* recorded them. Workspace ids,
    /// pane ids and `state_change_seq` are all session-scoped, so buckets from
    /// two sessions cannot be compared, appended, or drawn as one series however
    /// certain we are that they describe the same checkout.
    ///
    /// An unknown session matches only another unknown one. Two samples pulse
    /// could not attribute are two samples it cannot tell apart, and joining
    /// them would state a continuity nobody observed.
    ///
    /// Defaulted on load, so a file written before this field existed reads as
    /// one unattributable session rather than being claimed by the session that
    /// happens to be running now.
    #[serde(default)]
    pub session: Option<String>,
    /// Unix seconds at which that session started listening, when known. Carried
    /// so an interface can name the session a series belongs to without having
    /// watched it start.
    #[serde(default)]
    pub session_began: Option<u64>,
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
    /// `durable` is the key [`History::record`] resolved for this observation,
    /// not the path the observation carries: a path two workspaces in one
    /// snapshot share identifies neither of them, and writing it down anyway
    /// would make it a key again on the first sample where only one of the two is
    /// reported.
    ///
    /// `state_since` starts `None` on purpose. The state we are seeding with may
    /// have held for hours before this process started, and stamping it with
    /// "now" would report a five-second-old block on a workspace that has been
    /// waiting since lunch. We only know a duration once we have watched a
    /// change happen.
    fn new(
        observation: &WorkspaceObservation,
        durable: Option<&str>,
        session: Option<&SessionMark>,
        taken_at: u64,
        config: &Config,
    ) -> Self {
        Self {
            workspace_id: observation.workspace_id.clone(),
            label: observation.label.clone(),
            checkout_path: durable.map(str::to_string),
            session: session.map(|mark| mark.fingerprint.clone()),
            session_began: session.map(|mark| mark.began),
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

    /// Re-lays the ring at a coarser bucket size, folding `factor` old buckets
    /// into each new one, and returns how many folded buckets came out observed.
    ///
    /// Coarsening is arithmetic and nothing else. `bucket_number` is
    /// `at / bucket_seconds`, so when the new size is `factor` times the old one
    /// the old bucket `o` falls inside the new bucket `o / factor` — every old
    /// bucket lands wholly inside exactly one new bucket, and no observation has
    /// to be split or invented. A size that is *not* a whole multiple has no such
    /// mapping, and a smaller size would have to split one bucket into several,
    /// which is why [`load_from`] discards in both of those cases instead.
    ///
    /// **A fold containing a gap is a gap.** Counters are summed only when every
    /// old bucket in the group that could have been observed was observed; if any
    /// recorded minute inside it is missing, the new bucket is left unobserved.
    /// After the fold that bucket is the *only* record of its span, so calling
    /// the span observed because part of it was would launder unobserved time
    /// into a bar — the one lie this module exists to prevent. Summing rather
    /// than averaging is right for the same reason it is right in the sampler: a
    /// bucket counts observations, and five minutes of samples recorded at 300
    /// seconds is exactly the sum of five minutes recorded at 60.
    ///
    /// The newest group is the exception, and only at its leading edge: buckets
    /// past `newest_bucket` are the future rather than time nobody watched, so
    /// they are skipped instead of poisoning it. The alternative throws away the
    /// minutes already recorded in the bucket the user is looking at, every time
    /// the file is loaded.
    fn coarsen(&mut self, factor: u64, len: usize) -> usize {
        if factor <= 1 || len == 0 {
            return self.buckets.iter().filter(|b| b.observed()).count();
        }
        let newest = self.newest_bucket / factor;
        let mut fresh = vec![Bucket::default(); len];
        let oldest = newest.saturating_sub(len as u64 - 1);
        let mut kept = 0usize;
        for number in oldest..=newest {
            // No overflow to check on the way up: `number <= newest_bucket /
            // factor`, so `number * factor <= newest_bucket`.
            let first = number * factor;
            let mut folded = Bucket::default();
            let mut missed = false;
            for step in 0..factor {
                // The same division identity says `first + factor - 1` cannot
                // pass `newest_bucket` either, so this is belt and braces. It
                // costs nothing, and the alternative if that reasoning is ever
                // wrong is an arithmetic panic in somebody's sidebar.
                let Some(old) = first.checked_add(step) else {
                    break;
                };
                // Minutes after the newest one recorded are the future, not a
                // gap: nobody could have observed them, and the newest group is
                // always part-finished because time has not caught up with it
                // yet. Poisoning the fold with them would throw away the minutes
                // of the current bucket that *were* recorded, on every load.
                if old > self.newest_bucket {
                    break;
                }
                let old = self.bucket(old);
                if !old.observed() {
                    // A gap inside the recorded past, which the fold may not
                    // launder into a bar.
                    missed = true;
                    break;
                }
                folded.samples = folded.samples.saturating_add(old.samples);
                folded.working = folded.working.saturating_add(old.working);
                folded.blocked = folded.blocked.saturating_add(old.blocked);
                folded.transitions = folded.transitions.saturating_add(old.transitions);
            }
            // `first <= newest_bucket` always, so the first step of every group
            // is a real bucket: anything not `missed` has folded something.
            if !missed {
                fresh[(number % len as u64) as usize] = folded;
                kept += 1;
            }
        }
        self.buckets = fresh;
        self.newest_bucket = newest;
        kept
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
    ///
    /// Identity is resolved against the whole sample rather than one observation
    /// at a time, because a session that reissues ids can *swap* two of them: if
    /// `w15` and `w16` exchange checkouts, both entries must be re-keyed before
    /// either stale id is judged, or whichever was processed first would take
    /// the other's history down with it. See [`Self::locate`] and
    /// [`Self::drop_reused_ids`].
    pub fn record(&mut self, sample: &Sample, config: &Config) {
        let mut rewound = 0usize;
        let mut contradicted = 0usize;
        let mut live: Vec<usize> = Vec::new();
        for observation in &sample.workspaces {
            if !claims_one_id(observation, &sample.workspaces) {
                // One id reported twice in one snapshot. Both observations claim
                // the handle a badge is pushed to, so recording either would
                // decide which of two workspaces owns the other's sparkline.
                // Neither is recorded, and the entry already holding that id is
                // left exactly as it was: a stalled series that goes to gaps is
                // the honest reading of a snapshot that contradicts itself.
                contradicted += 1;
                continue;
            }
            let durable = durable_key(observation, &sample.workspaces);
            let (index, was_rewound) = self.record_workspace(
                observation,
                durable,
                sample.session.as_ref(),
                sample.taken_at,
                config,
            );
            if was_rewound {
                rewound += 1;
            }
            if !live.contains(&index) {
                live.push(index);
            }
        }
        if contradicted > 0 {
            eprintln!(
                "pulse: the snapshot reported {contradicted} workspace(s) under an id it \
                 also gave to another; recorded none of them"
            );
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
        let displaced = self.drop_reused_ids(&live, sample.session.as_ref());
        if displaced > 0 {
            eprintln!(
                "pulse: herdr reused {displaced} workspace id(s) for a different workspace; \
                 discarded the history recorded under them"
            );
        }
        // After, not before: a workspace introduced by this sample is the most
        // recently seen thing in the store and must never be the one evicted.
        self.evict(config, Some(sample.taken_at));
    }

    /// Folds one observation in, returning the entry it landed on and whether
    /// that entry's ring had to be re-anchored.
    fn record_workspace(
        &mut self,
        observation: &WorkspaceObservation,
        durable: Option<&str>,
        session: Option<&SessionMark>,
        taken_at: u64,
        config: &Config,
    ) -> (usize, bool) {
        let index = match self.locate(observation, durable, session) {
            Some(index) => {
                // The same workspace, so the series continues. Everything herdr
                // may have changed about it since the last sample — the
                // session-scoped id, the label, a checkout it did not report
                // before — is restamped onto the entry we already have.
                let entry = &mut self.workspaces[index];
                entry.workspace_id = observation.workspace_id.clone();
                entry.label = observation.label.clone();
                match durable {
                    Some(path) => entry.checkout_path = Some(path.to_string()),
                    // A path this sample refused must not stay on the entry as a
                    // key: the next sample that reports only one of the two
                    // workspaces sharing it would match on it and be handed the
                    // other's buckets. Ambiguity is not evidence of anything, so
                    // both fall back to the id and label until it clears.
                    None if observation.checkout_path.is_some() => entry.checkout_path = None,
                    // No worktree reported at all. Losing the evidence is not
                    // proof of a different workspace, so the recorded key stays
                    // and the workspace is still recognisable if it comes back.
                    None => {}
                }
                index
            }
            None => {
                // Not identifiable as anything on record: a new workspace, or an
                // id whose history belongs to somebody else. Either way this
                // starts an empty series, and [`Self::drop_reused_ids`] deals
                // with the entry still holding the id.
                self.workspaces.push(WorkspaceHistory::new(
                    observation,
                    durable,
                    session,
                    taken_at,
                    config,
                ));
                self.workspaces.len() - 1
            }
        };
        let rewound = self.workspaces[index].observe(observation, taken_at, config);
        (index, rewound)
    }

    /// The entry this observation is provably the continuation of, or `None` when
    /// no recorded workspace can be shown to be this one.
    ///
    /// The rule the whole module turns on applies here too: where identity cannot
    /// be established, drop rather than guess. A recovered series must be
    /// *provably* the same workspace, so the only two things that count as proof
    /// are the durable checkout path, and — for a workspace herdr reports no
    /// worktree for — an unchanged id and label together.
    ///
    /// Both are only ever looked for **within the sample's session**. A ring
    /// recorded by another herdr session is not a candidate however well its
    /// checkout matches: its ids and its `state_change_seq` values come from a
    /// counter that no longer exists, and appending to it would draw one
    /// unbroken watch across a boundary nobody watched across. The two questions
    /// compose — the path says which workspace, the session says which watch —
    /// and neither answers the other.
    ///
    /// Note what a durable key deliberately does *not* fall back to. An
    /// observation carrying a checkout path that matches no entry is not the
    /// workspace whose entry merely shares its id and label: two worktrees of one
    /// repo can carry the same label, and attributing one's buckets to the other
    /// is exactly the invisible wrong answer this module exists to refuse. The
    /// one exception is an entry with no recorded path at all, which is either a
    /// file written before this field existed or a workspace herdr has only just
    /// started reporting a worktree for — there the old evidence is all there
    /// ever was, and it is the same evidence the store used when it recorded it.
    fn locate(
        &self,
        observation: &WorkspaceObservation,
        durable: Option<&str>,
        session: Option<&SessionMark>,
    ) -> Option<usize> {
        let fingerprint = session.map(|mark| mark.fingerprint.as_str());
        let same_session = move |entry: &WorkspaceHistory| entry.session.as_deref() == fingerprint;
        let named = |entry: &WorkspaceHistory| {
            entry.workspace_id == observation.workspace_id && entry.label == observation.label
        };
        let find = |predicate: &dyn Fn(&WorkspaceHistory) -> bool| {
            self.workspaces
                .iter()
                .position(|entry| same_session(entry) && predicate(entry))
        };
        match durable {
            Some(path) => {
                find(&|entry: &WorkspaceHistory| entry.checkout_path.as_deref() == Some(path))
                    .or_else(|| {
                        find(&|entry: &WorkspaceHistory| {
                            entry.checkout_path.is_none() && named(entry)
                        })
                    })
            }
            None => find(&named),
        }
    }

    /// Drops the entries whose ids this sample proved belong to a different
    /// workspace, and returns how many.
    ///
    /// `live` are the entries this sample landed on, by index. An entry outside
    /// that set whose id one of them now holds has been displaced: herdr handed
    /// its id to a workspace we have just shown to be another one, and nothing
    /// remains that can identify it — a session-scoped id is the only handle a
    /// badge has, so it cannot be kept beside the workspace that now owns the id.
    ///
    /// Only entries of the *same* session are candidates. An id in a session that
    /// has ended was never a claim on this one: it is exactly as session-scoped
    /// as herdr says it is, and its ring is kept as that session's record until
    /// eviction ages it out.
    ///
    /// Deliberately once per sample rather than per observation: an id that looks
    /// displaced halfway through a sample may be re-keyed by a later observation
    /// in the same one, which is what a two-workspace swap looks like.
    fn drop_reused_ids(&mut self, live: &[usize], session: Option<&SessionMark>) -> usize {
        let fingerprint = session.map(|mark| mark.fingerprint.as_str());
        let claimed: Vec<String> = live
            .iter()
            .map(|index| self.workspaces[*index].workspace_id.clone())
            .collect();
        let before = self.workspaces.len();
        let kept: Vec<WorkspaceHistory> = std::mem::take(&mut self.workspaces)
            .into_iter()
            .enumerate()
            .filter(|(index, entry)| {
                live.contains(index)
                    || entry.session.as_deref() != fingerprint
                    || !claimed.contains(&entry.workspace_id)
            })
            .map(|(_, entry)| entry)
            .collect();
        self.workspaces = kept;
        before - self.workspaces.len()
    }

    /// Enforces the workspace cap, dropping the least recently seen first.
    ///
    /// This is half of the size bound — the ring bounds one workspace, this
    /// bounds how many rings exist — so it runs on every record and on every
    /// load, including when a config change lowers the cap under a file that was
    /// written with a higher one.
    ///
    /// One entry per workspace *per session* means a machine that restarts herdr
    /// all day accumulates rings for sessions that are over. They are the least
    /// recently seen, so they are the first to go: the cap falls on finished
    /// watches before it falls on one being sampled now.
    ///
    /// `now` is the time of the sample that prompted this, when there is one. An
    /// entry stamped *after* it was stamped by a clock that has since been
    /// corrected, and those go first however fresh they claim to be. Without
    /// that, a forward clock step followed by a correction would leave every
    /// ended session's ring claiming to be newer than the live one, and at the
    /// cap the workspace being sampled right now would be the one evicted —
    /// every cycle, until the wall clock climbed back past the stale stamps. A
    /// live workspace that can never accumulate a series is a sparkline of gaps
    /// with nothing to explain it.
    fn evict(&mut self, config: &Config, now: Option<u64>) {
        let cap = config.max_workspaces.max(1);
        let believable = |entry: &WorkspaceHistory| match now {
            Some(now) => entry.last_seen <= now,
            None => true,
        };
        while self.workspaces.len() > cap {
            let victim = self
                .workspaces
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    believable(a)
                        .cmp(&believable(b))
                        .then_with(|| a.last_seen.cmp(&b.last_seen))
                        // Ties broken by id and then by session so the same input
                        // always evicts the same entry, whatever the map iteration
                        // order upstream happened to be. The session is part of it
                        // because an id is only unique within one: two sessions
                        // sampled in the same second can otherwise tie on both
                        // fields and leave the choice to vector order.
                        .then_with(|| a.workspace_id.cmp(&b.workspace_id))
                        .then_with(|| a.session.cmp(&b.session))
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
        // reported both, so one of the two sparklines would quietly freeze. A
        // duplicate checkout path is the same failure through the durable key:
        // `locate` would find the first entry every time and the second would sit
        // there frozen, still being drawn. First occurrence wins in both cases.
        //
        // Both keys are scoped by session, because both are things herdr scopes
        // by session: one workspace's ring in this session and another's in a
        // session that has ended may legitimately carry the same id, and one
        // checkout may legitimately have been watched by two sessions in turn.
        // Deduplicating across sessions would delete a series that is not a
        // duplicate of anything.
        let mut ids: Vec<(Option<String>, String)> = Vec::new();
        let mut paths: Vec<(Option<String>, String)> = Vec::new();
        self.workspaces.retain(|workspace| {
            let id = (workspace.session.clone(), workspace.workspace_id.clone());
            if ids.contains(&id) {
                return false;
            }
            if let Some(path) = &workspace.checkout_path {
                let keyed = (workspace.session.clone(), path.clone());
                if paths.contains(&keyed) {
                    return false;
                }
                paths.push(keyed);
            }
            ids.push(id);
            true
        });
        for workspace in &mut self.workspaces {
            workspace.reshape(config.retention_buckets);
        }
        // No sample to measure against on a load: every stamp in the file is as
        // believable as every other until one arrives.
        self.evict(config, None);
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
                    // Provenance, so nothing downstream has to assume that two
                    // rows for one checkout are one interrupted series.
                    session: workspace.session.clone(),
                    session_began: workspace.session_began,
                }
            })
            .collect()
    }

    /// Serialised size in bytes, for the boundedness test.
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }

    /// Folds every ring onto a coarser bucket size, records the new size as the
    /// one this file is written at, and returns how many folded buckets came out
    /// of it observed.
    ///
    /// The count is what makes the message honest. A fold is only as good as the
    /// groups that were watched from end to end, and a factor large enough
    /// against a short recording can retain nothing at all — that is a discard
    /// however it happened, and it has to say so.
    fn coarsen(&mut self, factor: u64, config: &Config) -> usize {
        let mut kept = 0;
        for workspace in &mut self.workspaces {
            kept += workspace.coarsen(factor, config.retention_buckets);
        }
        self.bucket_seconds = config.bucket_seconds;
        kept
    }
}

/// This observation's durable identity, or `None` when it has none this sample
/// can rely on.
///
/// A checkout path identifies a workspace only while it names exactly one of
/// them. herdr will happily open two workspaces on the same checkout, and then
/// the path says "one of these two" — which is a guess, and a guess would merge
/// two workspaces' activity into one series under whichever label was seen last.
/// An ambiguous path is therefore no key at all, and both observations fall back
/// to the id and label they can still be told apart by.
fn durable_key<'a>(
    observation: &'a WorkspaceObservation,
    sample: &[WorkspaceObservation],
) -> Option<&'a str> {
    let path = observation.checkout_path.as_deref()?;
    let shared = sample
        .iter()
        .filter(|other| other.checkout_path.as_deref() == Some(path))
        .count();
    (shared == 1).then_some(path)
}

/// Whether this observation is the only one in its sample claiming its
/// `workspace_id`.
///
/// herdr should never report one id twice in a snapshot, and the store's own
/// invariant is that no two entries share one — an id is the handle a badge is
/// pushed to, so a second entry holding it would push a second sparkline at the
/// same workspace and the last write would win. A snapshot that contradicts
/// itself is not evidence about either workspace, so neither is recorded.
fn claims_one_id(observation: &WorkspaceObservation, sample: &[WorkspaceObservation]) -> bool {
    sample
        .iter()
        .filter(|other| other.workspace_id == observation.workspace_id)
        .count()
        == 1
}

/// How many recorded buckets fold into one at the live size, or `None` when the
/// history cannot be re-laid honestly.
///
/// `Some(factor)` only for a strict increase that is a whole multiple: those are
/// the sizes whose bucket boundaries still line up, so every recorded bucket
/// falls wholly inside exactly one new one. A decrease would have to split a
/// bucket into several, and a size that is not a whole multiple would have to
/// split at every boundary — in both cases the split is invention, and the
/// recorded history is discarded instead.
fn coarsening_factor(recorded: u64, live: u64) -> Option<u64> {
    // A width the sampler could never have written is not a scale to fold from.
    // `Config::clamp` pins every run to this range, so anything outside it came
    // from a hand-edited or damaged file, and the bucket numbers beside it were
    // computed at some scale we cannot know. Dividing them by a factor derived
    // from nonsense would re-lay real-looking bars in invented minutes; the file
    // takes the discard branch instead, like every other field we cannot
    // believe.
    if !(crate::config::MIN_BUCKET_SECONDS..=crate::config::MAX_BUCKET_SECONDS).contains(&recorded)
    {
        return None;
    }
    if live <= recorded || !live.is_multiple_of(recorded) {
        return None;
    }
    Some(live / recorded)
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
        match coarsening_factor(history.bucket_seconds, config.bucket_seconds) {
            Some(factor) => {
                let recorded = history.bucket_seconds;
                let kept = history.coarsen(factor, config);
                // Said out loud either way. A fold that kept something changed
                // the shape of every sparkline the user is about to read, and a
                // shape that changed for a reason nobody mentioned is a shape
                // people distrust. A fold that kept nothing is a discard however
                // it happened, and must not be reported as if history survived
                // it: a factor large against a short recording leaves no group
                // that was watched from end to end, and every one of them becomes
                // the gap it honestly is.
                if kept > 0 {
                    eprintln!(
                        "pulse: {} has {recorded}s buckets and this run uses {}s; \
                         folded every {factor} recorded buckets into one",
                        path.display(),
                        config.bucket_seconds
                    );
                } else {
                    eprintln!(
                        "pulse: starting with an empty history, {} has {recorded}s buckets and \
                         this run uses {}s; no run of {factor} buckets was watched end to end, \
                         so every fold of them is a gap",
                        path.display(),
                        config.bucket_seconds
                    );
                    // The message and the store have to agree. Every ring came
                    // back a gap, so what is left is workspace names attached to
                    // no observations at all; keeping them would draw rows that
                    // claim a last-seen time beside a sparkline with nothing in
                    // it, under a line that says the history is gone.
                    return History::empty(config);
                }
            }
            None => {
                // Three ways to get here, and the message says which. The new
                // size is smaller — one bucket cannot become several without
                // inventing detail nobody recorded. Or it is not a whole
                // multiple, so the new boundaries fall inside old buckets and
                // every fold would have to split observations it cannot split.
                // Or the width in the file is not one any run could have written,
                // which makes every bucket number beside it unreadable.
                let believable = (crate::config::MIN_BUCKET_SECONDS
                    ..=crate::config::MAX_BUCKET_SECONDS)
                    .contains(&history.bucket_seconds);
                let why = if !believable {
                    "no run of pulse could have written that bucket width, so the \
                     recorded bucket numbers cannot be placed in time"
                } else if config.bucket_seconds < history.bucket_seconds {
                    "a smaller bucket cannot be recovered from a larger one"
                } else {
                    "the new size is not a whole multiple of the old one"
                };
                eprintln!(
                    "pulse: starting with an empty history, {} has {}s buckets and this run \
                     uses {}s; {why}",
                    path.display(),
                    history.bucket_seconds,
                    config.bucket_seconds
                );
                return History::empty(config);
            }
        }
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
