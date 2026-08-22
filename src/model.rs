//! Shared types: the contract between the sampler, the store and the renderer.
//!
//! Owned by the integrator. The other modules read these types and none of them
//! change the definitions.

use std::fmt;

/// An agent's lifecycle state as herdr reports it.
///
/// The variants are exactly herdr's `AgentStatus` enum, read from the bundled
/// schema of a live 0.8.0 server (`herdr api schema`, `$defs/AgentStatus`):
/// `blocked | done | idle | unknown | working`. Note that the *write* side of
/// the protocol (`pane.report_agent`) accepts only four of these — plugins
/// cannot report `done` — but we only ever read.
///
/// An unrecognised string becomes [`AgentState::Unknown`] rather than being
/// dropped: a future herdr adding a sixth state must degrade to "we saw an
/// agent and could not classify it", never to "there was no agent here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentState {
    /// Waiting on a human. The most actionable state this plugin surfaces.
    Blocked,
    /// Doing work right now.
    Working,
    /// Alive, waiting for input, not blocked on anything in particular.
    Idle,
    /// Finished its turn.
    Done,
    /// Present but unclassifiable.
    Unknown,
}

impl AgentState {
    /// Parses herdr's wire spelling. Case-insensitive and whitespace-tolerant,
    /// because an empty string is how herdr reports absent context.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "blocked" => Self::Blocked,
            "working" => Self::Working,
            "idle" => Self::Idle,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }

    /// The wire spelling, for JSON output and round-trip tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    /// Rank used to reduce several agents in one workspace to a single state.
    /// Lower sorts first, and the winner is the *lowest* rank present.
    ///
    /// `Blocked` outranks `Working` deliberately, and this is the one place
    /// where we differ from herdr's own `workspaces[].agent_status`
    /// aggregation. A workspace with three busy agents and one waiting on a
    /// human needs to show the one that wants a human — that agent is the only
    /// one that will never make progress on its own.
    pub fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Working => 1,
            Self::Idle => 2,
            Self::Done => 3,
            Self::Unknown => 4,
        }
    }

    /// Every variant, for exhaustive tests and sweeps.
    pub const ALL: [AgentState; 5] = [
        Self::Blocked,
        Self::Working,
        Self::Idle,
        Self::Done,
        Self::Unknown,
    ];
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One agent as seen in a single `session.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentObservation {
    pub pane_id: String,
    pub workspace_id: String,
    /// The program (`claude`, `opencode`). herdr's `agents[]` entries carry no
    /// user-facing name field, verified against a live 0.8.0 snapshot, so this
    /// is the best label available.
    pub program: Option<String>,
    pub state: AgentState,
    /// herdr's `state_change_seq`: a **session-global** monotonic counter,
    /// stamped onto an agent at the moment it last changed state. Verified
    /// live — two agents that transitioned in the same second received 798 and
    /// 799 from one shared sequence.
    ///
    /// This is load-bearing. Comparing an agent's seq between two samples
    /// detects a transition that happened *between* those samples, including
    /// one that returned to the state we last saw. Comparing states alone would
    /// silently miss an agent that went working -> idle -> working inside one
    /// sampling interval, which is precisely the busy agent this plugin exists
    /// to distinguish from a wedged one.
    pub state_change_seq: u64,
}

/// One workspace as seen in a single `session.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceObservation {
    pub workspace_id: String,
    /// The user's label for the workspace.
    pub label: String,
    /// The absolute path of the checkout herdr has this workspace open on, from
    /// `workspaces[].worktree.checkout_path`, or `None` for a workspace with no
    /// worktree at all.
    ///
    /// This is the durable identity the sample history is keyed on. herdr's
    /// `workspace_id` is session-scoped and gets reused, whereas a checkout path
    /// is the same path tomorrow — so a workspace that comes back under a
    /// different id can still be recognised as the same one. `None` is not a
    /// weaker key, it is the absence of one: see `history::History::locate` for
    /// what identity means when herdr reports no worktree.
    pub checkout_path: Option<String>,
    pub agents: Vec<AgentObservation>,
}

impl WorkspaceObservation {
    /// The single state that best represents this workspace, by [`AgentState::rank`].
    ///
    /// A workspace with no agents at all is `Unknown`, not `Idle`: "nothing is
    /// running here" and "something is running here and resting" are different
    /// facts, and collapsing them would draw a quiet bar for an empty pane.
    pub fn state(&self) -> AgentState {
        self.agents
            .iter()
            .map(|agent| agent.state)
            .min_by_key(|state| state.rank())
            .unwrap_or(AgentState::Unknown)
    }

    /// Highest `state_change_seq` among this workspace's agents, or 0 when it
    /// has none.
    pub fn max_seq(&self) -> u64 {
        self.agents
            .iter()
            .map(|agent| agent.state_change_seq)
            .max()
            .unwrap_or(0)
    }
}

/// Which herdr session a snapshot came from, as far as pulse can establish it.
///
/// herdr reports no session identity of its own: `session.snapshot` carries a
/// version and a protocol number and nothing that distinguishes this run of the
/// server from the last one, and every `server.*` method is an action rather
/// than a read. So the identity is taken from the thing pulse is talking to —
/// the bound socket at `HERDR_SOCKET_PATH`.
///
/// What that proves and what it does not is the whole point of this type, and
/// `docs/herdr-protocol.md` records both. It is deliberately conservative: it
/// can report two sessions where there was one (a live handoff re-binds the
/// socket without ending the session), which costs a split in the history. The
/// opposite error — reporting one session where there were two — would join two
/// incomparable series, so the conservative direction is the correct one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMark {
    /// Opaque fingerprint of the socket: device, inode and creation time. Only
    /// ever compared for equality; nothing reads its parts.
    pub fingerprint: String,
    /// Unix seconds at which that socket was created, which is when this session
    /// started listening. Known without having watched it happen, so a series
    /// can say which session recorded it even on the sampler's first cycle.
    pub began: u64,
}

/// One complete `session.snapshot`, reduced to what this plugin records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Seconds since the Unix epoch, taken when the snapshot was received.
    pub taken_at: u64,
    /// The session this snapshot was read from, or `None` when it could not be
    /// established.
    ///
    /// `None` is not "the same session as last time". Two samples pulse cannot
    /// attribute are two samples it cannot tell apart, and the store treats an
    /// unknown session as its own unnameable one rather than folding it into a
    /// named series it might not belong to.
    pub session: Option<SessionMark>,
    pub workspaces: Vec<WorkspaceObservation>,
}

/// Which badge token name a workspace's current state should light.
///
/// herdr renders a token's value as flat text and cannot colour by content, so
/// severity is encoded in the **token name** and the user gives each name its
/// own `fg` in `config.toml`. Exactly one is lit at a time; a flip must clear
/// the previous name first, or the merge-patch semantics leave two badges lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// At least one agent is waiting on a human.
    Blocked,
    /// At least one agent is working.
    Working,
    /// Nothing is working and nothing is blocked.
    Quiet,
}

impl Tone {
    pub fn of(state: AgentState) -> Self {
        match state {
            AgentState::Blocked => Self::Blocked,
            AgentState::Working => Self::Working,
            AgentState::Idle | AgentState::Done | AgentState::Unknown => Self::Quiet,
        }
    }

    pub fn token_name(self) -> &'static str {
        match self {
            Self::Blocked => "pulse_blocked",
            Self::Working => "pulse_working",
            Self::Quiet => "pulse_quiet",
        }
    }

    /// Every token name this plugin may ever set, so `--disable` can sweep them
    /// all without knowing what was lit.
    pub const ALL_TOKENS: [&'static str; 3] = ["pulse_blocked", "pulse_working", "pulse_quiet"];
}

/// A workspace's history plus its current state, ready to render.
///
/// This is what the renderer receives: the store answers "what happened", the
/// sampler answers "what is true now", and this joins them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceActivity {
    pub workspace_id: String,
    pub label: String,
    /// Oldest first, one entry per bucket, ending with the bucket that contains
    /// `as_of`. A `None` is a **gap**: the sampler was not running then. It is
    /// not a quiet period, and must never render as one.
    pub series: Vec<Option<Level>>,
    pub state: AgentState,
    /// How long `state` has held, in seconds, **measured to [`Self::last_seen`]
    /// rather than to now**. `None` when we have not observed a transition yet
    /// and so genuinely do not know.
    ///
    /// The distinction matters once the sampler stops. Measuring to now would
    /// turn "an agent was working when we last looked, five hours ago" into the
    /// confident claim "an agent has been working for five hours" — a
    /// present-tense assertion about a period nobody observed, sitting in the
    /// same row as a sparkline full of gaps that says the opposite.
    pub state_for: Option<u64>,
    /// Unix seconds of the most recent observation of this workspace, or `None`
    /// if it has never been observed.
    ///
    /// This is what makes `state` falsifiable. `state` is always a *past*
    /// observation; without knowing how old it is, a reader cannot tell a live
    /// fact from a stale one, and every renderer would have to guess.
    pub last_seen: Option<u64>,
    pub agent_count: usize,
    /// Fingerprint of the herdr session that recorded this series, or `None`
    /// when the session could not be established.
    ///
    /// Two rows for one workspace with different fingerprints are two series,
    /// not one interrupted one: workspace ids, pane ids and `state_change_seq`
    /// are all session-scoped, so nothing recorded under one session is
    /// comparable with anything recorded under another.
    pub session: Option<String>,
    /// Unix seconds at which that session started listening, when known.
    pub session_began: Option<u64>,
    /// The coarse series: 28 columns of six hours each, covering the last week,
    /// oldest first. Same rules as [`Self::series`] — `None` is a gap, `0` is
    /// observed and quiet — and recorded from the same samples rather than folded
    /// out of the fine ring, which cannot reach back that far.
    pub week: Vec<Option<Level>>,
    /// Seconds an agent was observed blocked, over the window [`Self::series`]
    /// covers.
    ///
    /// Estimated from the samples that saw a blocked agent, which is the only
    /// thing the store records — and reported next to [`Self::watched_seconds`]
    /// for a reason. Alone it invites the reading "blocked ten minutes in the
    /// last four hours", which is false when only twenty minutes of those four
    /// hours were watched. Together they say what was seen and how much watching
    /// it rests on.
    pub blocked_seconds: u64,
    /// Seconds of the same window the sampler actually observed. A gap adds
    /// nothing to it: unobserved time is not time with no blocking in it.
    pub watched_seconds: u64,
    /// The same two figures over the week series' window, so a week row can
    /// report the week's blocked time rather than this afternoon's.
    ///
    /// Two pairs rather than one for the same reason there are two series: the
    /// rings cover different stretches of time, and a figure from one drawn
    /// beside the other's sparkline is a number about a period the reader is not
    /// looking at.
    pub week_blocked_seconds: u64,
    pub week_watched_seconds: u64,
}

impl WorkspaceActivity {
    /// How long ago this workspace was last observed, relative to `as_of`.
    /// `None` when it has never been observed.
    ///
    /// Saturating, so a history file written by a clock ahead of ours reports
    /// "just now" rather than underflowing into an enormous age.
    pub fn observed_ago(&self, as_of: u64) -> Option<u64> {
        self.last_seen.map(|seen| as_of.saturating_sub(seen))
    }

    /// Whether [`Self::state`] is recent enough to state in the present tense.
    ///
    /// `tolerance` is how far behind the sampler is allowed to be — a couple of
    /// sampling intervals, so an ordinary missed cycle does not flip every row
    /// to "stale", while a stopped daemon does so quickly.
    pub fn is_current(&self, as_of: u64, tolerance: u64) -> bool {
        self.observed_ago(as_of).is_some_and(|ago| ago <= tolerance)
    }

    /// Whether this series was recorded by `mark`'s session.
    ///
    /// An unknown session matches only another unknown one, and never a named
    /// one: two samples pulse could not attribute are not evidence of a shared
    /// session, and treating them as one is the join this type exists to
    /// prevent.
    pub fn is_session(&self, mark: Option<&SessionMark>) -> bool {
        self.session.as_deref() == mark.map(|mark| mark.fingerprint.as_str())
    }
}

/// A bucket's activity, normalised to the sparkline's scale.
///
/// `0` is "observed, and nothing happened" — distinct from a gap, which is
/// `None` at the [`WorkspaceActivity::series`] level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(pub u8);

impl Level {
    /// Inclusive upper bound. Eight steps because that is what the block-element
    /// ramp `▁▂▃▄▅▆▇█` offers; a ninth would have nowhere to go.
    pub const MAX: u8 = 8;

    pub fn new(raw: u8) -> Self {
        Self(raw.min(Self::MAX))
    }

    pub fn is_quiet(self) -> bool {
        self.0 == 0
    }
}
