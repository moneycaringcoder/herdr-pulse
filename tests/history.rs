//! Historian tests.
//!
//! The module's three load-bearing properties get the most vectors here:
//! boundedness, gaps that stay gaps, and a persistence path that cannot poison
//! the next run. Everything else is a degenerate case that would otherwise turn
//! into an invisible wrong answer — a clock stepping backwards, a ring slot left
//! over from a previous lap, a workspace id reused by a new session.
//!
//! Fixtures come from `tests/data/snapshot-live.json`, a real herdr 0.8.0
//! snapshot with values sanitised and structure left byte-exact, rather than
//! from what the shape of a snapshot ought to be.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pulse::config::Config;
use pulse::history::{self, Bucket, History, FORMAT_VERSION};
use pulse::model::{
    AgentObservation, AgentState, Level, Sample, SessionMark, WorkspaceObservation,
};

/// A minute-aligned instant, so a test that adds 60 seconds moves exactly one
/// bucket and a test that adds 5 does not.
const T0: u64 = 1_699_999_980;

fn config(bucket_seconds: u64, retention_buckets: usize, max_workspaces: usize) -> Config {
    Config {
        bucket_seconds,
        retention_buckets,
        max_workspaces,
        ..Config::default()
    }
}

fn workspace(id: &str, label: &str, agents: &[(&str, &str, u64)]) -> WorkspaceObservation {
    WorkspaceObservation {
        workspace_id: id.to_string(),
        label: label.to_string(),
        // No worktree, which is the majority case in the captured snapshot:
        // seven of its ten workspaces. These have no durable key, so they
        // exercise the id-and-label rule.
        checkout_path: None,
        agents: agents
            .iter()
            .map(|(pane, state, seq)| AgentObservation {
                pane_id: pane.to_string(),
                workspace_id: id.to_string(),
                program: Some("claude".to_string()),
                state: AgentState::parse(state),
                state_change_seq: *seq,
            })
            .collect(),
    }
}

/// A workspace herdr reports a worktree for, so the store has a durable key to
/// recognise it by across a reused id.
fn checkout(
    id: &str,
    label: &str,
    path: &str,
    agents: &[(&str, &str, u64)],
) -> WorkspaceObservation {
    WorkspaceObservation {
        checkout_path: Some(path.to_string()),
        ..workspace(id, label, agents)
    }
}

/// The session every helper stamps a sample with unless a test says otherwise.
///
/// Named rather than unknown, because production practically always has a mark:
/// the socket the snapshot was read from is right there to stat. A suite that
/// defaulted to "session unknown" would be testing the degenerate path
/// everywhere and the ordinary one nowhere.
fn session_a() -> SessionMark {
    SessionMark {
        fingerprint: "2049:1001:1699990000:0".to_string(),
        began: T0 - 3600,
    }
}

/// A second herdr session: a different socket, bound later.
fn session_b() -> SessionMark {
    SessionMark {
        fingerprint: "2049:2002:1699999900:0".to_string(),
        began: T0 - 60,
    }
}

/// A sample of a single workspace, which is what most of these tests need.
fn one(taken_at: u64, id: &str, label: &str, agents: &[(&str, &str, u64)]) -> Sample {
    sample(taken_at, vec![workspace(id, label, agents)])
}

fn sample(taken_at: u64, workspaces: Vec<WorkspaceObservation>) -> Sample {
    sample_in(taken_at, Some(session_a()), workspaces)
}

/// A sample attributed to a named session, or to none at all.
fn sample_in(
    taken_at: u64,
    session: Option<SessionMark>,
    workspaces: Vec<WorkspaceObservation>,
) -> Sample {
    Sample {
        taken_at,
        session,
        workspaces,
    }
}

/// The bucket a workspace is currently writing into.
fn newest_bucket(history: &History, index: usize) -> Bucket {
    let workspace = &history.workspaces[index];
    let slot = (workspace.newest_bucket % workspace.buckets.len() as u64) as usize;
    workspace.buckets[slot]
}

/// One column per bucket, ending at `as_of`. The shape most of the projection
/// assertions want.
fn series(history: &History, as_of: u64, columns: usize, config: &Config) -> Vec<Option<Level>> {
    history.activity(as_of, columns, 1, config)[0]
        .series
        .clone()
}

/// A throwaway directory that removes itself, so persistence tests never share
/// state and never depend on `HERDR_PLUGIN_STATE_DIR` — a process-global that
/// tests running on parallel threads would fight over.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pulse-history-{}-{unique}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The captured snapshot, reduced the way the sampler reduces one.
///
/// This deliberately does not call `herdr::reduce_snapshot` — that belongs to
/// another module and is still unimplemented. Reading the fixture here keeps
/// these tests pinned to the real wire shape (arrays under `result.snapshot`,
/// agents joined to workspaces by `workspace_id`) without waiting on it.
fn live_sample(taken_at: u64) -> Sample {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/snapshot-live.json");
    let raw = std::fs::read_to_string(&path).expect("read the captured snapshot");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse the captured snapshot");
    let snapshot = &value["result"]["snapshot"];

    let mut workspaces: Vec<WorkspaceObservation> = snapshot["workspaces"]
        .as_array()
        .expect("workspaces array")
        .iter()
        .map(|workspace| WorkspaceObservation {
            workspace_id: workspace["workspace_id"].as_str().unwrap().to_string(),
            label: workspace["label"].as_str().unwrap().to_string(),
            // Three of the ten fixture workspaces carry a worktree and seven
            // carry `null`, exactly as captured. That split is the point: a
            // fixture where every workspace had a durable key would not exercise
            // the workspaces that have none.
            checkout_path: workspace["worktree"]["checkout_path"]
                .as_str()
                .map(str::to_string),
            agents: Vec::new(),
        })
        .collect();

    for agent in snapshot["agents"].as_array().expect("agents array") {
        let workspace_id = agent["workspace_id"].as_str().unwrap().to_string();
        let observation = AgentObservation {
            pane_id: agent["pane_id"].as_str().unwrap().to_string(),
            workspace_id: workspace_id.clone(),
            program: agent["agent"].as_str().map(str::to_string),
            state: AgentState::parse(agent["agent_status"].as_str().unwrap_or("")),
            state_change_seq: agent["state_change_seq"].as_u64().unwrap_or(0),
        };
        if let Some(workspace) = workspaces
            .iter_mut()
            .find(|workspace| workspace.workspace_id == workspace_id)
        {
            workspace.agents.push(observation);
        }
    }

    sample(taken_at, workspaces)
}

fn live_workspace(sample: &Sample, id: &str) -> WorkspaceObservation {
    sample
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == id)
        .expect("the fixture has this workspace")
        .clone()
}

// ---------------------------------------------------------------------------
// Degenerate shapes
// ---------------------------------------------------------------------------

#[test]
fn an_empty_history_projects_nothing() {
    let config = config(60, 8, 4);
    let history = History::empty(&config);

    assert_eq!(history.version, FORMAT_VERSION);
    assert_eq!(history.bucket_seconds, 60);
    assert!(history.workspaces.is_empty());
    assert!(history.activity(T0, 8, 1, &config).is_empty());
}

#[test]
fn a_sample_with_no_workspaces_records_nothing() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(&sample(T0, Vec::new()), &config);

    assert!(history.workspaces.is_empty());
}

#[test]
fn one_sample_lights_one_column_and_leaves_the_rest_gaps() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    let series = series(&history, T0, 8, &config);
    assert_eq!(series.len(), 8);
    assert!(
        series[..7].iter().all(Option::is_none),
        "everything before the one observed minute is a gap: {series:?}"
    );
    assert!(series[7].is_some());
}

#[test]
fn a_zero_length_ring_is_all_gaps_rather_than_a_panic() {
    // Not reachable through `config::load`, which clamps, but a hand-edited file
    // can carry one and no input may panic the sampler.
    let config = config(60, 0, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    history.record(
        &one(T0 + 600, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    assert!(series(&history, T0 + 600, 8, &config)
        .iter()
        .all(Option::is_none));
}

#[test]
fn zero_columns_is_an_empty_series() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    assert!(history.activity(T0, 0, 1, &config)[0].series.is_empty());
}

#[test]
fn zero_buckets_per_column_behaves_as_one() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    let widened = history.activity(T0, 8, 0, &config)[0].series.clone();
    assert_eq!(widened, series(&history, T0, 8, &config));
}

#[test]
fn a_workspace_with_no_agents_is_observed_and_unknown() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(&one(T0, "w1", "alpha", &[]), &config);

    // "Nothing is running here" is an observation, not a gap. A gap would claim
    // the sampler was off, which it demonstrably was not.
    let activity = &history.activity(T0, 4, 1, &config)[0];
    assert_eq!(activity.series[3], Some(Level(0)));
    assert_eq!(activity.state, AgentState::Unknown);
    assert_eq!(activity.agent_count, 0);
    assert_eq!(newest_bucket(&history, 0).samples, 1);
}

// ---------------------------------------------------------------------------
// Gaps
// ---------------------------------------------------------------------------

#[test]
fn an_observed_quiet_bucket_is_zero_never_a_gap() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);

    let series = series(&history, T0, 4, &config);
    assert_eq!(
        series[3],
        Some(Level(0)),
        "an idle minute was watched and must not read as a gap"
    );
    assert!(series[3].unwrap().is_quiet());
}

#[test]
fn a_gap_in_the_middle_of_a_series_stays_a_gap() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    // Five minutes of the daemon being down.
    history.record(
        &one(T0 + 5 * 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    let series = series(&history, T0 + 5 * 60, 8, &config);
    assert!(series[7].is_some(), "the newest minute is data");
    assert!(series[2].is_some(), "the first sample is five columns back");
    for (column, level) in series.iter().enumerate() {
        if column != 2 && column != 7 {
            assert_eq!(*level, None, "column {column} was never observed");
        }
    }
}

#[test]
fn a_window_with_nothing_recorded_in_it_is_entirely_gaps() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    // Forty minutes later, with a ring only eight buckets long: everything in
    // the window predates nothing and postdates everything, and every ring slot
    // still physically holds the sample above.
    let series = series(&history, T0 + 40 * 60, 8, &config);
    assert!(
        series.iter().all(Option::is_none),
        "a lap-old ring slot is not data: {series:?}"
    );
}

#[test]
fn a_daemon_that_was_never_running_for_the_window_reports_gaps_not_quiet() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);

    // The window ends an hour after the only sample, well inside retention.
    let series = series(&history, T0 + 60 * 60, 8, &config);
    assert!(
        series.iter().all(Option::is_none),
        "the last eight minutes were not observed: {series:?}"
    );
}

#[test]
fn a_partially_observed_column_is_data_not_a_gap() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    // One observed minute inside a four-minute column.
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    let activity = &history.activity(T0 + 3 * 60, 2, 4, &config)[0];
    assert!(
        activity.series[1].is_some(),
        "a column with one observed bucket out of four is real data"
    );
}

#[test]
fn a_column_averages_only_its_observed_buckets() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    // Two minutes of a four-minute column, both fully busy. Averaging over four
    // would halve a genuinely busy column because the daemon started late.
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    let solid = &history.activity(T0 + 3 * 60, 1, 4, &config)[0].series[0];
    let mut full = History::empty(&config);
    for minute in 0..4 {
        full.record(
            &one(T0 + minute * 60, "w1", "alpha", &[("w1:p1", "working", 10)]),
            &config,
        );
    }
    let reference = &full.activity(T0 + 3 * 60, 1, 4, &config)[0].series[0];
    assert_eq!(solid, reference);
}

// ---------------------------------------------------------------------------
// The ring
// ---------------------------------------------------------------------------

#[test]
fn samples_in_the_same_minute_land_in_the_same_bucket() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    for step in 0..12 {
        history.record(
            &one(
                T0 + step * 5,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + step)],
            ),
            &config,
        );
    }

    let bucket = newest_bucket(&history, 0);
    assert_eq!(bucket.samples, 12);
    assert_eq!(bucket.working, 12);
    // Eleven of the twelve samples saw the seq move.
    assert_eq!(bucket.transitions, 11);
    assert_eq!(history.workspaces[0].buckets.len(), 8);
}

#[test]
fn occupancy_counts_samples_not_agents() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[
                ("w1:p1", "working", 10),
                ("w1:p2", "working", 11),
                ("w1:p3", "working", 12),
                ("w1:p4", "working", 13),
            ],
        ),
        &config,
    );

    let bucket = newest_bucket(&history, 0);
    assert_eq!(bucket.samples, 1);
    assert_eq!(
        bucket.working, 1,
        "one sample saw working agents, however many there were"
    );
}

#[test]
fn a_sample_a_bucket_or_two_behind_is_dropped_as_ordinary_jitter() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for minute in 0..4 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    let before = history.clone();
    // Two buckets behind the newest: the sample straddled a boundary, or the
    // snapshot round trip was slow. Re-opening a minute we have finished
    // reporting is not worth it, and nothing is frozen by declining to.
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "idle", 99)]),
        &config,
    );

    assert_eq!(
        history, before,
        "a sample within the jitter window must change nothing at all"
    );
}

#[test]
fn a_backwards_sample_within_the_same_bucket_is_still_recorded() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(T0 + 30, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    // Ten seconds backwards, but the same minute: no history is rewritten by
    // accepting it, and dropping it would lose resolution for nothing.
    history.record(
        &one(T0 + 20, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).samples, 2);
    assert_eq!(history.workspaces[0].last_seen, T0 + 30);
}

#[test]
fn ring_slots_from_a_previous_lap_are_stale_not_data() {
    let config = config(60, 4, 4);
    let mut history = History::empty(&config);
    // Fill every slot of a four-bucket ring with busy minutes.
    for minute in 0..4 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }
    // Then jump a whole lap forward, so every slot the walk passes over holds a
    // busy minute from the previous lap. The seq is the one last seen, so the
    // newest minute is genuinely quiet and any bar in the series is stale data
    // rather than churn.
    let jumped = T0 + 7 * 60;
    history.record(
        &one(jumped, "w1", "alpha", &[("w1:p1", "idle", 13)]),
        &config,
    );

    let series = series(&history, jumped, 4, &config);
    assert_eq!(
        series,
        vec![None, None, None, Some(Level(0))],
        "the three skipped minutes were not observed and the ring must say so"
    );
}

#[test]
fn a_ring_that_wrapped_many_times_keeps_exactly_the_newest_lap() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    // Twelve laps of a ring eight buckets long, alternating busy and quiet so a
    // misaligned slot would show up as a phase error rather than as nothing.
    for minute in 0..96u64 {
        let state = if minute % 2 == 0 { "working" } else { "idle" };
        history.record(
            &one(T0 + minute * 60, "w1", "alpha", &[("w1:p1", state, 10)]),
            &config,
        );
    }

    let as_of = T0 + 95 * 60;
    let recent = series(&history, as_of, 8, &config);
    assert!(
        recent.iter().all(Option::is_some),
        "the newest lap is entirely observed: {recent:?}"
    );
    for (column, level) in recent.iter().enumerate() {
        // Column 7 is minute 95, which is odd and therefore quiet.
        let busy = (95 - (7 - column)) % 2 == 0;
        assert_eq!(
            level.unwrap().is_quiet(),
            !busy,
            "column {column} has the wrong phase"
        );
    }

    let older = history.activity(as_of, 16, 1, &config)[0].series.clone();
    assert!(
        older[..8].iter().all(Option::is_none),
        "anything older than one lap has been dropped: {older:?}"
    );
    assert_eq!(history.workspaces[0].buckets.len(), 8);
}

#[test]
fn the_ring_never_grows_however_many_samples_arrive() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for step in 0..5_000u64 {
        history.record(
            &one(T0 + step * 5, "w1", "alpha", &[("w1:p1", "working", step)]),
            &config,
        );
    }

    assert_eq!(history.workspaces[0].buckets.len(), 16);
    assert_eq!(history.workspaces[0].agent_seqs.len(), 1);
}

#[test]
fn counters_saturate_rather_than_wrap() {
    // A wrapped counter reads as a suddenly quiet workspace: a wrong answer with
    // nothing on screen to suggest it.
    let config = config(3_600, 4, 4);
    let mut history = History::empty(&config);

    for step in 0..70_000u64 {
        history.record(
            &one(T0, "w1", "alpha", &[("w1:p1", "working", step)]),
            &config,
        );
    }

    let bucket = newest_bucket(&history, 0);
    assert_eq!(bucket.samples, u16::MAX);
    assert_eq!(bucket.working, u16::MAX);
    assert_eq!(bucket.transitions, u16::MAX);
}

// ---------------------------------------------------------------------------
// Boundedness
// ---------------------------------------------------------------------------

#[test]
fn many_thousands_of_samples_stay_under_a_size_ceiling() {
    // Twenty-four workspaces, four agents each, sampled every five seconds for
    // just over eight hours: twice around a four-hour ring.
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);

    let ceiling = 2 * 1024 * 1024;
    let mut at_one_lap = 0;
    for step in 0..6_000u64 {
        let taken_at = T0 + step * 5;
        let workspaces = (0..24)
            .map(|index| {
                let id = format!("w{index}");
                let pane = |slot: u64| format!("{id}:p{slot}");
                WorkspaceObservation {
                    workspace_id: id.clone(),
                    label: format!("workspace-{index}"),
                    checkout_path: Some(format!("/home/dev/repos/project-{index}")),
                    agents: (0..4)
                        .map(|slot| AgentObservation {
                            pane_id: pane(slot),
                            workspace_id: id.clone(),
                            program: Some("claude".to_string()),
                            state: if (step + index + slot) % 3 == 0 {
                                AgentState::Working
                            } else {
                                AgentState::Idle
                            },
                            state_change_seq: step * 7 + index + slot,
                        })
                        .collect(),
                }
            })
            .collect();
        history.record(&sample(taken_at, workspaces), &config);

        if step == 2_880 {
            at_one_lap = history.encoded_len();
        }
    }

    let after_two_laps = history.encoded_len();
    assert!(
        after_two_laps < ceiling,
        "{after_two_laps} bytes is over the {ceiling} byte ceiling"
    );
    assert!(
        after_two_laps <= at_one_lap + 4_096,
        "size must stop growing once the ring is full: {at_one_lap} -> {after_two_laps}"
    );
    assert_eq!(history.workspaces.len(), 24);
    for workspace in &history.workspaces {
        assert_eq!(workspace.buckets.len(), 240);
        assert_eq!(workspace.agent_seqs.len(), 4);
    }
}

#[test]
fn what_gets_dropped_is_the_oldest_data() {
    let config = config(60, 32, 8);
    let mut history = History::empty(&config);

    // A hundred minutes of samples through a ring that holds thirty-two.
    for minute in 0..100u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    let as_of = T0 + 99 * 60;
    let window = history.activity(as_of, 100, 1, &config)[0].series.clone();
    let (dropped, kept) = window.split_at(100 - 32);
    assert!(
        dropped.iter().all(Option::is_none),
        "the sixty-eight oldest minutes are gone"
    );
    assert!(
        kept.iter().all(Option::is_some),
        "the thirty-two newest minutes are all still here"
    );
    assert_eq!(history.workspaces[0].newest_bucket, (as_of) / 60);
}

#[test]
fn workspaces_beyond_the_cap_are_evicted_least_recently_seen_first() {
    let config = config(60, 8, 3);
    let mut history = History::empty(&config);

    let all = vec![
        workspace("wA", "alpha", &[("wA:p1", "idle", 1)]),
        workspace("wB", "beta", &[("wB:p1", "idle", 2)]),
        workspace("wC", "gamma", &[("wC:p1", "idle", 3)]),
    ];
    history.record(&sample(T0, all), &config);

    // A and B are seen again; C is now the least recently seen.
    history.record(
        &sample(
            T0 + 60,
            vec![
                workspace("wA", "alpha", &[("wA:p1", "idle", 1)]),
                workspace("wB", "beta", &[("wB:p1", "idle", 2)]),
            ],
        ),
        &config,
    );
    history.record(
        &one(T0 + 120, "wD", "delta", &[("wD:p1", "idle", 4)]),
        &config,
    );

    let ids: Vec<&str> = history
        .workspaces
        .iter()
        .map(|w| w.workspace_id.as_str())
        .collect();
    assert_eq!(ids, vec!["wA", "wB", "wD"]);
}

#[test]
fn a_workspace_introduced_by_this_sample_is_never_the_one_evicted() {
    let config = config(60, 8, 2);
    let mut history = History::empty(&config);
    history.record(
        &sample(
            T0,
            vec![
                workspace("wA", "alpha", &[("wA:p1", "idle", 1)]),
                workspace("wB", "beta", &[("wB:p1", "idle", 2)]),
            ],
        ),
        &config,
    );

    history.record(
        &one(T0 + 60, "wC", "gamma", &[("wC:p1", "idle", 3)]),
        &config,
    );

    let ids: Vec<&str> = history
        .workspaces
        .iter()
        .map(|w| w.workspace_id.as_str())
        .collect();
    assert_eq!(ids, vec!["wB", "wC"]);
}

#[test]
fn far_more_workspaces_than_the_cap_stay_capped() {
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);

    for step in 0..200u64 {
        let workspaces = (0..40)
            .map(|index| {
                workspace(
                    &format!("w{index}"),
                    &format!("workspace-{index}"),
                    &[("p1", "working", step)],
                )
            })
            .collect();
        history.record(&sample(T0 + step * 5, workspaces), &config);
        assert!(history.workspaces.len() <= 8);
    }

    assert_eq!(history.workspaces.len(), 8);
    assert!(history.encoded_len() < 128 * 1024);
}

#[test]
fn eviction_breaks_ties_deterministically() {
    let config = config(60, 8, 2);
    let mut first = History::empty(&config);
    let mut second = History::empty(&config);

    let forwards = vec![
        workspace("wA", "alpha", &[("wA:p1", "idle", 1)]),
        workspace("wB", "beta", &[("wB:p1", "idle", 2)]),
        workspace("wC", "gamma", &[("wC:p1", "idle", 3)]),
    ];
    let mut backwards = forwards.clone();
    backwards.reverse();

    first.record(&sample(T0, forwards), &config);
    second.record(&sample(T0, backwards), &config);

    let ids = |history: &History| -> Vec<String> {
        let mut ids: Vec<String> = history
            .workspaces
            .iter()
            .map(|w| w.workspace_id.clone())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&first), ids(&second));
}

#[test]
fn an_evicted_workspace_that_returns_starts_from_nothing() {
    let config = config(60, 8, 1);
    let mut history = History::empty(&config);

    history.record(&one(T0, "wA", "alpha", &[("wA:p1", "working", 1)]), &config);
    history.record(
        &one(T0 + 60, "wB", "beta", &[("wB:p1", "working", 2)]),
        &config,
    );
    history.record(
        &one(T0 + 120, "wA", "alpha", &[("wA:p1", "working", 3)]),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    let series = series(&history, T0 + 120, 4, &config);
    assert_eq!(
        series,
        vec![None, None, None, series[3]],
        "the returning workspace has only the minute it came back in"
    );
    assert!(series[3].is_some());
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_that_disappears_and_returns_keeps_its_history_with_a_gap() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in [0u64, 1] {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }
    // Two minutes in which the workspace was closed. Other workspaces keep the
    // daemon running, so the absence is real information — but it is information
    // about a workspace that was not there, which is a gap, not a quiet minute.
    for minute in [2u64, 3] {
        history.record(
            &one(T0 + minute * 60, "w2", "beta", &[("w2:p1", "idle", 50)]),
            &config,
        );
    }
    history.record(
        &one(T0 + 4 * 60, "w1", "alpha", &[("w1:p1", "working", 12)]),
        &config,
    );

    let series = series(&history, T0 + 4 * 60, 5, &config);
    assert!(series[0].is_some());
    assert!(series[1].is_some());
    assert_eq!(series[2], None);
    assert_eq!(series[3], None);
    assert!(series[4].is_some());
}

#[test]
fn a_reused_workspace_id_with_a_new_label_drops_the_old_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w15",
                "old-project",
                &[("w15:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }
    // A new herdr session hands `w15` to somebody else.
    history.record(
        &one(
            T0 + 5 * 60,
            "w15",
            "new-project",
            &[("w15:p1", "idle", 900)],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(history.workspaces[0].label, "new-project");
    let activity = &history.activity(T0 + 5 * 60, 6, 1, &config)[0];
    assert_eq!(activity.label, "new-project");
    assert_eq!(
        activity.series[..5].iter().filter(|c| c.is_some()).count(),
        0,
        "the previous tenant's minutes must not be attributed here: {:?}",
        activity.series
    );
    assert_eq!(activity.series[5], Some(Level(0)));
    assert_eq!(
        activity.state_for, None,
        "a fresh workspace has no observed transition to measure from"
    );
}

#[test]
fn an_unchanged_label_keeps_the_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w15",
                "same-project",
                &[("w15:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    let series = series(&history, T0 + 4 * 60, 5, &config);
    assert!(series.iter().all(Option::is_some), "{series:?}");
}

// ---------------------------------------------------------------------------
// Identity: which workspace a recorded series belongs to
// ---------------------------------------------------------------------------

/// The ids the store is holding, which must never repeat: an id is what a badge
/// is pushed to, so two entries sharing one would push two sparklines at one
/// workspace.
fn ids(history: &History) -> Vec<&str> {
    history
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.as_str())
        .collect()
}

fn assert_ids_are_unique(history: &History) {
    let mut seen = ids(history);
    let total = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        total,
        "duplicate workspace id in {:?}",
        ids(history)
    );
}

#[test]
fn a_renamed_workspace_keeps_its_history_when_the_checkout_is_the_same() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![checkout(
                    "w15",
                    "old-name",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
    }
    // A rename, not a reuse: same session, same id, same checkout.
    history.record(
        &sample(
            T0 + 5 * 60,
            vec![checkout(
                "w15",
                "new-name",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 20)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(history.workspaces[0].label, "new-name");
    let series = series(&history, T0 + 5 * 60, 6, &config);
    assert!(
        series.iter().all(Option::is_some),
        "a rename is not a different workspace: {series:?}"
    );
}

#[test]
fn a_workspace_that_comes_back_under_a_new_id_keeps_its_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
    }
    // A fresh herdr session numbers the same workspace differently.
    history.record(
        &sample(
            T0 + 5 * 60,
            vec![checkout(
                "w3",
                "api",
                "/home/dev/repos/api",
                &[("w3:p1", "working", 4)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(
        history.workspaces[0].workspace_id, "w3",
        "the entry has to follow the id herdr is using now, or the badge goes nowhere"
    );
    let series = series(&history, T0 + 5 * 60, 6, &config);
    assert!(
        series.iter().all(Option::is_some),
        "the checkout is the same, so the series continues: {series:?}"
    );
}

#[test]
fn an_id_reused_by_another_checkout_under_a_new_label_drops_the_displaced_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
    }
    // A new session hands `w15` to a workspace on a different checkout, and the
    // one we were recording is nowhere in this sample.
    history.record(
        &sample(
            T0 + 5 * 60,
            vec![checkout(
                "w15",
                "web",
                "/home/dev/repos/web",
                &[("w15:p1", "idle", 900)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_ids_are_unique(&history);
    assert_eq!(
        history.workspaces[0].checkout_path.as_deref(),
        Some("/home/dev/repos/web")
    );
    let activity = &history.activity(T0 + 5 * 60, 6, 1, &config)[0];
    assert_eq!(activity.label, "web");
    assert_eq!(
        activity.series[..5].iter().filter(|c| c.is_some()).count(),
        0,
        "the previous tenant's minutes must not be attributed here: {:?}",
        activity.series
    );
    assert_eq!(activity.series[5], Some(Level(0)));
}

#[test]
fn an_id_reused_under_the_same_label_but_a_different_checkout_drops_the_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    // Two linked worktrees of one repo, which herdr labels alike. The label
    // agreeing is exactly why this case needs the path: on the label alone these
    // two would look like one workspace.
    for minute in 0..5u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![checkout(
                    "w15",
                    "project",
                    "/home/dev/.herdr/worktrees/project/one",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
    }
    history.record(
        &sample(
            T0 + 5 * 60,
            vec![checkout(
                "w15",
                "project",
                "/home/dev/.herdr/worktrees/project/two",
                &[("w15:p1", "working", 40)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(
        history.workspaces[0].checkout_path.as_deref(),
        Some("/home/dev/.herdr/worktrees/project/two")
    );
    let activity = &history.activity(T0 + 5 * 60, 6, 1, &config)[0];
    assert_eq!(
        activity.series[..5].iter().filter(|c| c.is_some()).count(),
        0,
        "same label, different checkout: the minutes belong to the other worktree: {:?}",
        activity.series
    );
}

#[test]
fn two_workspaces_that_swap_ids_keep_their_own_series() {
    // Whichever order the swapped observations arrive in, both series survive:
    // judging a displaced id before the whole sample has been read would drop
    // the workspace that is about to claim it.
    for reversed in [false, true] {
        let config = config(60, 16, 4);
        let mut history = History::empty(&config);
        for minute in 0..4u64 {
            history.record(
                &sample(
                    T0 + minute * 60,
                    vec![
                        checkout(
                            "w15",
                            "api",
                            "/home/dev/repos/api",
                            &[("w15:p1", "working", 10 + minute)],
                        ),
                        checkout(
                            "w16",
                            "web",
                            "/home/dev/repos/web",
                            &[("w16:p1", "idle", 500)],
                        ),
                    ],
                ),
                &config,
            );
        }

        let mut swapped = vec![
            checkout(
                "w16",
                "api",
                "/home/dev/repos/api",
                &[("w16:p1", "working", 90)],
            ),
            checkout(
                "w15",
                "web",
                "/home/dev/repos/web",
                &[("w15:p1", "idle", 91)],
            ),
        ];
        if reversed {
            swapped.reverse();
        }
        history.record(&sample(T0 + 4 * 60, swapped), &config);

        assert_eq!(history.workspaces.len(), 2, "reversed: {reversed}");
        assert_ids_are_unique(&history);
        let activity = history.activity(T0 + 4 * 60, 5, 1, &config);
        let api = activity.iter().find(|a| a.label == "api").expect("api");
        let web = activity.iter().find(|a| a.label == "web").expect("web");
        assert_eq!(api.workspace_id, "w16", "reversed: {reversed}");
        assert_eq!(web.workspace_id, "w15", "reversed: {reversed}");
        assert!(
            api.series.iter().all(|c| c.is_some_and(|l| !l.is_quiet())),
            "reversed: {reversed}, {:?}",
            api.series
        );
        assert!(
            web.series.iter().all(|c| c == &Some(Level(0))),
            "reversed: {reversed}, {:?}",
            web.series
        );
    }
}

#[test]
fn a_checkout_path_two_workspaces_share_is_not_an_identity() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    // herdr will open two workspaces on one checkout, and then the path says
    // "one of these two", which is a guess. Falling back to the id keeps the two
    // series apart instead of merging them under whichever label came last.
    for minute in 0..4u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![
                    checkout(
                        "wA",
                        "first",
                        "/home/dev/repos/shared",
                        &[("wA:p1", "working", 10 + minute)],
                    ),
                    checkout(
                        "wB",
                        "second",
                        "/home/dev/repos/shared",
                        &[("wB:p1", "idle", 500)],
                    ),
                ],
            ),
            &config,
        );
    }

    assert_eq!(history.workspaces.len(), 2);
    assert_ids_are_unique(&history);
    for entry in &history.workspaces {
        assert_eq!(
            entry.checkout_path, None,
            "a path this sample refused must not be recorded as a key: {entry:?}"
        );
    }
    let activity = history.activity(T0 + 3 * 60, 4, 1, &config);
    let first = activity.iter().find(|a| a.label == "first").expect("first");
    let second = activity
        .iter()
        .find(|a| a.label == "second")
        .expect("second");
    assert!(first
        .series
        .iter()
        .all(|c| c.is_some_and(|l| !l.is_quiet())));
    assert!(second.series.iter().all(|c| c == &Some(Level(0))));
}

#[test]
fn the_survivor_of_a_shared_checkout_does_not_inherit_the_other_series() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    // Four minutes with both workspaces on one checkout: one working throughout,
    // one idle throughout.
    for minute in 0..4u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![
                    checkout(
                        "wA",
                        "first",
                        "/home/dev/repos/shared",
                        &[("wA:p1", "working", 10 + minute)],
                    ),
                    checkout(
                        "wB",
                        "second",
                        "/home/dev/repos/shared",
                        &[("wB:p1", "idle", 500)],
                    ),
                ],
            ),
            &config,
        );
    }
    // The busy one closes, so the path is unambiguous again — and it must not
    // hand the idle workspace four minutes of somebody else's work.
    history.record(
        &sample(
            T0 + 4 * 60,
            vec![checkout(
                "wB",
                "second",
                "/home/dev/repos/shared",
                &[("wB:p1", "idle", 500)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 2, "{:?}", ids(&history));
    assert_ids_are_unique(&history);
    let activity = history.activity(T0 + 4 * 60, 5, 1, &config);
    let second = activity
        .iter()
        .find(|a| a.label == "second")
        .expect("second");
    assert!(
        second.series.iter().all(|c| c == &Some(Level(0))),
        "the idle workspace was idle for every observed minute: {:?}",
        second.series
    );
    let first = activity.iter().find(|a| a.label == "first").expect("first");
    assert_eq!(
        first.series[4], None,
        "the closed workspace was not observed in the last minute: {:?}",
        first.series
    );
    assert!(
        first.series[..4]
            .iter()
            .all(|c| c.is_some_and(|l| !l.is_quiet())),
        "and it keeps the minutes it did work: {:?}",
        first.series
    );
}

#[test]
fn a_workspace_whose_worktree_stops_being_reported_keeps_its_history() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..5u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
    }
    // The worktree goes away — the checkout was deleted, or herdr stopped
    // resolving it. Losing the evidence is not proof of a different workspace,
    // and the id and label still agree, so the series continues.
    history.record(
        &one(T0 + 5 * 60, "w15", "api", &[("w15:p1", "working", 40)]),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    let series = series(&history, T0 + 5 * 60, 6, &config);
    assert!(series.iter().all(Option::is_some), "{series:?}");
}

#[test]
fn a_ring_with_no_recorded_path_is_adopted_by_the_first_worktree_its_session_reports() {
    let dir = TempDir::new("pre-checkout-file");
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    for minute in 0..5u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w15",
                "api",
                &[("w15:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    // A ring recorded in this session before herdr reported a worktree for the
    // workspace: the same shape a file from before the field existed has, with
    // the session still attributed. What an actual pre-release file does — no
    // session either, so no named session adopts it — is
    // `a_history_file_written_before_sessions_is_not_claimed_by_the_live_session`.
    let mut value = serde_json::to_value(&history).unwrap();
    let entry = value["workspaces"][0].as_object_mut().unwrap();
    assert!(
        entry.remove("checkout_path").is_some(),
        "the field is written"
    );
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let mut loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded.workspaces.len(), 1);
    assert_eq!(loaded.workspaces[0].checkout_path, None);

    // The first sample of that same session carrying a worktree adopts the entry
    // on the evidence it was recorded with, and stamps the durable key onto it.
    loaded.record(
        &sample(
            T0 + 5 * 60,
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 40)],
            )],
        ),
        &config,
    );

    assert_eq!(loaded.workspaces.len(), 1);
    assert_eq!(
        loaded.workspaces[0].checkout_path.as_deref(),
        Some("/home/dev/repos/api")
    );
    let series = series(&loaded, T0 + 5 * 60, 6, &config);
    assert!(
        series.iter().all(Option::is_some),
        "a worktree appearing must not cost the recorded history: {series:?}"
    );
}

#[test]
fn two_entries_on_one_checkout_path_do_not_survive_a_load() {
    let dir = TempDir::new("duplicate-checkout");
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    history.record(
        &sample(
            T0,
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 10)],
            )],
        ),
        &config,
    );

    // A hand-edited file: one checkout under two ids. `locate` would find the
    // first every time, leaving the second frozen and still being drawn.
    let mut value = serde_json::to_value(&history).unwrap();
    let mut clone = value["workspaces"][0].clone();
    clone["workspace_id"] = serde_json::json!("w16");
    value["workspaces"].as_array_mut().unwrap().push(clone);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded.workspaces.len(), 1);
    assert_eq!(loaded.workspaces[0].workspace_id, "w15");
}

#[test]
fn a_snapshot_that_reports_one_id_twice_records_neither_workspace() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    history.record(
        &sample(
            T0,
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 10)],
            )],
        ),
        &config,
    );

    // herdr should never do this. If it does, both observations claim the id a
    // badge is pushed to, and recording either would decide which workspace owns
    // the other's sparkline.
    history.record(
        &sample(
            T0 + 60,
            vec![
                checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 11)],
                ),
                checkout(
                    "w15",
                    "web",
                    "/home/dev/repos/web",
                    &[("w15:p2", "working", 12)],
                ),
            ],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_ids_are_unique(&history);
    assert_eq!(
        history.workspaces[0].checkout_path.as_deref(),
        Some("/home/dev/repos/api"),
        "the entry already holding the id is left as it was"
    );
    let series = series(&history, T0 + 60, 2, &config);
    assert_eq!(
        series[1], None,
        "a self-contradicting snapshot observed nothing: {series:?}"
    );
}

// ---------------------------------------------------------------------------
// Which session recorded a series
// ---------------------------------------------------------------------------

/// Records four minutes of one working workspace on a checkout, in `session`.
fn watched(config: &Config, session: Option<SessionMark>, minutes: u64) -> History {
    let mut history = History::empty(config);
    for minute in 0..minutes {
        history.record(
            &sample_in(
                T0 + minute * 60,
                session.clone(),
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            config,
        );
    }
    history
}

#[test]
fn a_new_session_records_beside_the_old_series_and_not_into_it() {
    let config = config(60, 16, 4);
    let mut history = watched(&config, Some(session_a()), 4);

    // herdr is restarted. Same checkout, same label, and it happens to hand out
    // the same id — but the counter behind `state_change_seq` is a new one, and
    // nothing recorded under the old session is comparable with this.
    history.record(
        &sample_in(
            T0 + 4 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 3)],
            )],
        ),
        &config,
    );

    assert_eq!(
        history.workspaces.len(),
        2,
        "two watches of one checkout are two series: {:?}",
        history
            .workspaces
            .iter()
            .map(|w| (w.session.clone(), w.workspace_id.clone()))
            .collect::<Vec<_>>()
    );
    let old = &history.workspaces[0];
    let new = &history.workspaces[1];
    assert_eq!(
        old.session.as_deref(),
        Some(session_a().fingerprint.as_str())
    );
    assert_eq!(
        new.session.as_deref(),
        Some(session_b().fingerprint.as_str())
    );
    assert_eq!(new.session_began, Some(session_b().began));

    let activity = history.activity(T0 + 4 * 60, 5, 1, &config);
    let earlier = activity
        .iter()
        .find(|a| a.session.as_deref() == Some(session_a().fingerprint.as_str()))
        .expect("the earlier session is still reported");
    let live = activity
        .iter()
        .find(|a| a.session.as_deref() == Some(session_b().fingerprint.as_str()))
        .expect("the live session");

    // The old session keeps the minutes it observed — they are real, and
    // blanking them would claim nobody was watching when somebody was.
    assert!(
        earlier.series[..4].iter().all(Option::is_some),
        "the earlier watch keeps its own minutes: {:?}",
        earlier.series
    );
    assert_eq!(
        earlier.series[4], None,
        "and claims nothing about the minute it did not see: {:?}",
        earlier.series
    );
    // The new session claims only what it watched.
    assert!(
        live.series[..4].iter().all(Option::is_none),
        "a session cannot claim minutes recorded before it began: {:?}",
        live.series
    );
    assert!(live.series[4].is_some());
}

#[test]
fn a_new_session_does_not_manufacture_transitions_from_a_reset_counter() {
    let config = config(60, 16, 4);
    let mut history = watched(&config, Some(session_a()), 3);

    // `state_change_seq` is session-global and a fresh server starts it over, so
    // the same pane id arrives with a number that has nothing to do with the one
    // we stored. Comparing them would count a transition nobody made — and pane
    // ids are session-scoped too, so it might not even be the same agent.
    history.record(
        &sample_in(
            T0 + 3 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 1)],
            )],
        ),
        &config,
    );

    let fresh = history
        .workspaces
        .iter()
        .position(|w| w.session.as_deref() == Some(session_b().fingerprint.as_str()))
        .expect("the new session's ring");
    assert_eq!(
        newest_bucket(&history, fresh).transitions,
        0,
        "a first sighting in a new session is not evidence of a transition"
    );
}

#[test]
fn an_unknown_session_never_joins_a_named_one() {
    let config = config(60, 16, 4);
    let mut history = watched(&config, Some(session_a()), 3);

    // The socket could not be attributed. That is not "the session we already
    // know about" — it is a watch pulse cannot place.
    history.record(
        &sample_in(
            T0 + 3 * 60,
            None,
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 20)],
            )],
        ),
        &config,
    );
    assert_eq!(history.workspaces.len(), 2);

    // Two unattributable samples in a row are the same unattributable watch, or
    // the store would grow one ring per sample and record nothing useful at all.
    history.record(
        &sample_in(
            T0 + 4 * 60,
            None,
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 21)],
            )],
        ),
        &config,
    );
    assert_eq!(history.workspaces.len(), 2);
    let unknown = history
        .workspaces
        .iter()
        .find(|w| w.session.is_none())
        .expect("the unattributable ring");
    assert_eq!(unknown.session_began, None);
    assert_eq!(unknown.last_seen, T0 + 4 * 60);
}

#[test]
fn a_history_file_written_before_sessions_is_not_claimed_by_the_live_session() {
    let dir = TempDir::new("pre-session-file");
    let config = config(60, 16, 4);
    let history = watched(&config, Some(session_a()), 4);

    // Exactly what the previous release wrote: no session fields at all.
    let mut value = serde_json::to_value(&history).unwrap();
    let entry = value["workspaces"][0].as_object_mut().unwrap();
    assert!(entry.remove("session").is_some(), "the field is written");
    entry.remove("session_began");
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let mut loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded.workspaces.len(), 1);
    assert_eq!(
        loaded.workspaces[0].session, None,
        "a ring whose session was never recorded is unattributable, not ours"
    );

    loaded.record(
        &sample_in(
            T0 + 4 * 60,
            Some(session_a()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 30)],
            )],
        ),
        &config,
    );

    assert_eq!(
        loaded.workspaces.len(),
        2,
        "the upgrade keeps the old buckets and starts an attributed series beside them"
    );
    assert!(
        loaded.workspaces[0].buckets.iter().any(|b| b.samples > 0),
        "the pre-session buckets are still there"
    );
}

#[test]
fn one_id_may_belong_to_two_sessions_at_once() {
    let config = config(60, 16, 4);
    let mut history = watched(&config, Some(session_a()), 2);

    // A session-scoped id is exactly that. `w15` in the new session is a
    // different workspace from `w15` in the old one, and the store holds both
    // without either being a duplicate of anything.
    history.record(
        &sample_in(
            T0 + 2 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "docs",
                "/home/dev/repos/docs",
                &[("w15:p1", "idle", 4)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 2);
    assert!(history.workspaces.iter().all(|w| w.workspace_id == "w15"));
    let labels: Vec<&str> = history
        .workspaces
        .iter()
        .map(|w| w.label.as_str())
        .collect();
    assert_eq!(labels, ["api", "docs"]);
}

#[test]
fn a_reused_id_inside_a_session_leaves_another_sessions_ring_alone() {
    let config = config(60, 16, 4);
    let mut history = watched(&config, Some(session_a()), 3);

    // Session B watches the same checkout for a while...
    history.record(
        &sample_in(
            T0 + 3 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 2)],
            )],
        ),
        &config,
    );
    // ...and then hands `w15` to a different checkout. That displaces B's own
    // ring, and must not touch A's.
    history.record(
        &sample_in(
            T0 + 4 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "web",
                "/home/dev/repos/web",
                &[("w15:p1", "idle", 9)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 2, "{:?}", ids(&history));
    let earlier = history
        .workspaces
        .iter()
        .find(|w| w.session.as_deref() == Some(session_a().fingerprint.as_str()))
        .expect("session A's ring survives");
    assert_eq!(
        earlier.checkout_path.as_deref(),
        Some("/home/dev/repos/api")
    );
    assert_eq!(
        earlier.buckets.iter().filter(|b| b.samples > 0).count(),
        3,
        "session A recorded three minutes and keeps all three"
    );
    let live = history
        .workspaces
        .iter()
        .find(|w| w.session.as_deref() == Some(session_b().fingerprint.as_str()))
        .expect("session B's ring");
    assert_eq!(live.checkout_path.as_deref(), Some("/home/dev/repos/web"));
}

#[test]
fn two_sessions_watching_one_checkout_do_not_mix_their_series() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    for minute in 0..4u64 {
        // The same checkout, watched by two sessions in the same minutes, busy
        // under one and quiet under the other. Nothing in one series may leak
        // into the other.
        history.record(
            &sample_in(
                T0 + minute * 60,
                Some(session_a()),
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 10 + minute)],
                )],
            ),
            &config,
        );
        history.record(
            &sample_in(
                T0 + minute * 60,
                Some(session_b()),
                vec![checkout(
                    "w9",
                    "api",
                    "/home/dev/repos/api",
                    &[("w9:p1", "idle", 500)],
                )],
            ),
            &config,
        );
    }

    let activity = history.activity(T0 + 3 * 60, 4, 1, &config);
    assert_eq!(activity.len(), 2);
    let busy = activity
        .iter()
        .find(|a| a.session.as_deref() == Some(session_a().fingerprint.as_str()))
        .expect("session A");
    let quiet = activity
        .iter()
        .find(|a| a.session.as_deref() == Some(session_b().fingerprint.as_str()))
        .expect("session B");
    assert!(busy.series.iter().all(|c| c.is_some_and(|l| !l.is_quiet())));
    assert!(quiet.series.iter().all(|c| c == &Some(Level(0))));
}

#[test]
fn a_series_says_which_session_recorded_it() {
    let config = config(60, 16, 4);
    let history = watched(&config, Some(session_a()), 2);

    let activity = &history.activity(T0 + 60, 2, 1, &config)[0];
    assert_eq!(
        activity.session.as_deref(),
        Some(session_a().fingerprint.as_str())
    );
    assert_eq!(activity.session_began, Some(session_a().began));
    assert!(
        activity.is_session(Some(&session_a())),
        "the live session recognises its own series"
    );
    assert!(
        !activity.is_session(Some(&session_b())),
        "and does not adopt another session's"
    );
    assert!(
        !activity.is_session(None),
        "an unknown live session claims nothing"
    );
}

#[test]
fn an_ended_session_is_evicted_before_a_live_workspace() {
    // Two rings, one live workspace: the cap has to fall on the watch that is
    // over, not on the one being sampled right now.
    let config = config(60, 16, 1);
    let mut history = watched(&config, Some(session_a()), 2);
    history.record(
        &sample_in(
            T0 + 5 * 60,
            Some(session_b()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 1)],
            )],
        ),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(
        history.workspaces[0].session.as_deref(),
        Some(session_b().fingerprint.as_str()),
        "the live session's ring is the one that survives the cap"
    );
}

#[test]
fn session_churn_stays_bounded_and_never_evicts_the_live_watch() {
    // A machine that restarts herdr all day: forty sessions, six workspaces
    // each. One entry per (workspace, session) is the whole point of this
    // feature and also the way it could grow without limit, so the cap has to
    // hold and it has to fall on the finished watches.
    let config = config(60, 240, 24);
    let mut history = History::empty(&config);
    for run in 0..40u64 {
        let session = SessionMark {
            fingerprint: format!("2049:{}:{}:0", 1000 + run, T0 + run * 600),
            began: T0 + run * 600,
        };
        for minute in 0..3u64 {
            let taken_at = T0 + run * 600 + minute * 60;
            let workspaces = (0..6)
                .map(|index| {
                    checkout(
                        &format!("w{index}"),
                        &format!("workspace-{index}"),
                        &format!("/home/dev/repos/project-{index}"),
                        &[(&format!("w{index}:p1"), "working", 10 + minute)],
                    )
                })
                .collect();
            history.record(
                &sample_in(taken_at, Some(session.clone()), workspaces),
                &config,
            );
        }

        assert!(
            history.workspaces.len() <= config.max_workspaces,
            "run {run} left {} entries under a cap of {}",
            history.workspaces.len(),
            config.max_workspaces
        );
        // Every workspace of the session being sampled is still there, whatever
        // the cap had to throw away.
        let live = history
            .workspaces
            .iter()
            .filter(|entry| entry.session.as_deref() == Some(session.fingerprint.as_str()))
            .count();
        assert_eq!(live, 6, "run {run} lost a live workspace to the cap");
    }

    // And the file stays inside the ceiling the boundedness test guards.
    let encoded = history.encoded_len();
    assert!(
        encoded < 2 * 1024 * 1024,
        "{encoded} bytes of session churn is over the ceiling"
    );
}

#[test]
fn a_stale_future_stamp_does_not_starve_the_live_watch_at_the_cap() {
    // A forward clock step, then a correction. The ended session's ring keeps the
    // stamp the stepped clock wrote, which is now in the future; the live ring
    // carries the corrected time. At the cap, ranking by `last_seen` alone would
    // evict the workspace being sampled right now — every cycle, until the wall
    // clock caught up with a time that never happened.
    let config = config(60, 16, 1);
    let mut history = History::empty(&config);
    history.record(
        &sample_in(
            T0 + 10 * 3_600,
            Some(session_a()),
            vec![checkout(
                "w15",
                "api",
                "/home/dev/repos/api",
                &[("w15:p1", "working", 10)],
            )],
        ),
        &config,
    );

    // The clock is corrected and herdr is restarted.
    for minute in 0..3u64 {
        history.record(
            &sample_in(
                T0 + minute * 60,
                Some(session_b()),
                vec![checkout(
                    "w15",
                    "api",
                    "/home/dev/repos/api",
                    &[("w15:p1", "working", 1 + minute)],
                )],
            ),
            &config,
        );
    }

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(
        history.workspaces[0].session.as_deref(),
        Some(session_b().fingerprint.as_str()),
        "the ring being sampled has to survive a stamp from a clock that was wrong"
    );
    let series = series(&history, T0 + 2 * 60, 3, &config);
    assert!(
        series.iter().any(Option::is_some),
        "and it has to be able to accumulate a series: {series:?}"
    );
}

#[test]
fn two_workspaces_do_not_mix_their_series() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    for minute in 0..4u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![
                    workspace("wA", "busy", &[("wA:p1", "working", 10 + minute)]),
                    workspace("wB", "quiet", &[("wB:p1", "idle", 500)]),
                ],
            ),
            &config,
        );
    }

    let activity = history.activity(T0 + 3 * 60, 4, 1, &config);
    let busy = activity.iter().find(|a| a.label == "busy").unwrap();
    let quiet = activity.iter().find(|a| a.label == "quiet").unwrap();
    assert!(busy.series.iter().all(|c| c.is_some_and(|l| !l.is_quiet())));
    assert!(quiet.series.iter().all(|c| c == &Some(Level(0))));
}

#[test]
fn workspaces_are_reported_in_the_order_they_were_first_seen() {
    let config = config(60, 8, 8);
    let mut history = History::empty(&config);

    history.record(&one(T0, "wZ", "zulu", &[("wZ:p1", "idle", 1)]), &config);
    history.record(
        &one(T0 + 60, "wA", "alpha", &[("wA:p1", "idle", 2)]),
        &config,
    );

    let labels: Vec<&str> = history
        .activity(T0 + 60, 4, 1, &config)
        .iter()
        .map(|a| a.label.clone())
        .map(|label| Box::leak(label.into_boxed_str()) as &str)
        .collect();
    assert_eq!(labels, vec!["zulu", "alpha"]);
}

// ---------------------------------------------------------------------------
// Transitions and state
// ---------------------------------------------------------------------------

#[test]
fn a_moved_seq_is_a_transition_even_when_the_state_is_unchanged() {
    // The whole reason `state_change_seq` is recorded. An agent that went
    // working -> idle -> working between two samples looks identical to one that
    // never moved, unless the seq is compared.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]),
        &config,
    );
    history.record(
        &one(T0 + 5, "w1", "alpha", &[("w1:p1", "working", 799)]),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).transitions, 1);
}

#[test]
fn a_delta_in_the_global_seq_is_not_a_transition_count() {
    // The counter is session-global, so a jump of forty includes every other
    // workspace's transitions. One agent moving is one transition, whatever the
    // size of the step.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 700)]),
        &config,
    );
    history.record(
        &one(T0 + 5, "w1", "alpha", &[("w1:p1", "working", 740)]),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).transitions, 1);
}

#[test]
fn an_unchanged_seq_is_not_a_transition() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    for step in 0..5u64 {
        history.record(
            &one(T0 + step * 5, "w1", "alpha", &[("w1:p1", "working", 795)]),
            &config,
        );
    }

    assert_eq!(newest_bucket(&history, 0).transitions, 0);
}

#[test]
fn a_first_sighting_is_not_a_transition() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]),
        &config,
    );
    assert_eq!(newest_bucket(&history, 0).transitions, 0);

    // A second agent appearing is also a first sighting, not a transition.
    history.record(
        &one(
            T0 + 5,
            "w1",
            "alpha",
            &[("w1:p1", "working", 795), ("w1:p2", "idle", 800)],
        ),
        &config,
    );
    assert_eq!(newest_bucket(&history, 0).transitions, 0);
}

#[test]
fn a_seq_that_went_backwards_is_still_a_transition() {
    // herdr restarting begins a fresh session-global sequence. Treating a lower
    // number as "no movement" would go quiet until the new session climbed past
    // the old one, which could be hours.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]),
        &config,
    );
    history.record(
        &one(T0 + 5, "w1", "alpha", &[("w1:p1", "working", 3)]),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).transitions, 1);
}

#[test]
fn transitions_sum_across_the_workspaces_agents() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[
                ("w1:p1", "working", 10),
                ("w1:p2", "working", 11),
                ("w1:p3", "working", 12),
            ],
        ),
        &config,
    );
    history.record(
        &one(
            T0 + 5,
            "w1",
            "alpha",
            &[
                ("w1:p1", "working", 20),
                ("w1:p2", "working", 11),
                ("w1:p3", "working", 21),
            ],
        ),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).transitions, 2);
}

#[test]
fn blocked_and_working_are_both_recorded_for_one_sample() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[("w1:p1", "working", 10), ("w1:p2", "blocked", 11)],
        ),
        &config,
    );

    let bucket = newest_bucket(&history, 0);
    assert_eq!(bucket.working, 1);
    assert_eq!(bucket.blocked, 1);
    assert_eq!(
        history.activity(T0, 1, 1, &config)[0].state,
        AgentState::Blocked,
        "the agent waiting on a human is the one worth surfacing"
    );
}

#[test]
fn an_unrecognised_state_is_unknown_rather_than_absent() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "reticulating", 10)]),
        &config,
    );

    let activity = &history.activity(T0, 1, 1, &config)[0];
    assert_eq!(activity.state, AgentState::Unknown);
    assert_eq!(activity.agent_count, 1);
    assert_eq!(newest_bucket(&history, 0).samples, 1);
}

#[test]
fn how_long_a_state_has_held_is_unknown_until_a_change_is_watched() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    assert_eq!(
        history.activity(T0, 4, 1, &config)[0].state_for,
        None,
        "this agent may have been idle since breakfast; we started watching now"
    );

    // The transition is watched at T0+120, and the workspace is then watched
    // holding that state until T0+180: one observed minute of working.
    history.record(
        &one(T0 + 120, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    history.record(
        &one(T0 + 180, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    let activity = &history.activity(T0 + 180, 4, 1, &config)[0];
    assert_eq!(activity.state, AgentState::Working);
    assert_eq!(activity.state_for, Some(60));
    assert_eq!(activity.last_seen, Some(T0 + 180));
}

#[test]
fn a_state_that_holds_keeps_its_original_start() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    for minute in 2..6u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 11 + minute)],
            ),
            &config,
        );
    }

    assert_eq!(
        history.activity(T0 + 5 * 60, 4, 1, &config)[0].state_for,
        Some(4 * 60)
    );
}

#[test]
fn a_state_duration_never_runs_backwards() {
    // A `state_since` after `last_seen` is only reachable through a hand-edited
    // file, and an unchecked subtraction there reports a duration of several
    // hundred billion years with complete confidence.
    let dir = TempDir::new("impossible-duration");
    let config = config(60, 16, 8);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    let mut value = serde_json::to_value(&history).unwrap();
    value["workspaces"][0]["state_since"] = serde_json::json!(T0 + 10 * 60);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    assert_eq!(
        loaded.activity(T0 + 60, 4, 1, &config)[0].state_for,
        Some(0)
    );
}

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

#[test]
fn levels_rise_with_occupancy() {
    let config = config(60, 8, 4);
    let mut previous = Level(0);
    for busy in 0..=12u64 {
        let mut history = History::empty(&config);
        for step in 0..12u64 {
            let state = if step < busy { "working" } else { "idle" };
            history.record(
                &one(T0 + step * 5, "w1", "alpha", &[("w1:p1", state, 10)]),
                &config,
            );
        }
        let level = series(&history, T0, 1, &config)[0].expect("observed");
        assert!(
            level >= previous,
            "level fell from {previous:?} to {level:?} at {busy}/12 busy samples"
        );
        previous = level;
    }
    assert_eq!(
        previous,
        Level(6),
        "a fully busy bucket leaves churn headroom"
    );
}

#[test]
fn churn_lifts_a_bucket_that_never_caught_an_agent_working() {
    // Twelve idle samples with the seq moving under them: the agent has been
    // finishing turns between samples, which is not a quiet minute.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for step in 0..12u64 {
        history.record(
            &one(
                T0 + step * 5,
                "w1",
                "alpha",
                &[("w1:p1", "idle", 10 + step)],
            ),
            &config,
        );
    }

    let level = series(&history, T0, 1, &config)[0].expect("observed");
    assert!(
        !level.is_quiet(),
        "eleven state changes is not a quiet minute"
    );
}

#[test]
fn a_level_never_exceeds_the_ramp() {
    let config = config(3_600, 8, 4);
    let mut history = History::empty(&config);
    for step in 0..500u64 {
        history.record(
            &one(T0 + step * 5, "w1", "alpha", &[("w1:p1", "working", step)]),
            &config,
        );
    }

    let level = series(&history, T0, 1, &config)[0].expect("observed");
    assert!(level.0 <= Level::MAX, "{level:?} is off the ramp");
    assert_eq!(level, Level(8));
}

// ---------------------------------------------------------------------------
// Projection geometry
// ---------------------------------------------------------------------------

#[test]
fn the_last_column_contains_as_of_and_the_first_is_oldest() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    // Only the oldest minute of an eight-minute window is busy.
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    for minute in 1..8u64 {
        history.record(
            &one(T0 + minute * 60, "w1", "alpha", &[("w1:p1", "idle", 10)]),
            &config,
        );
    }

    let series = series(&history, T0 + 7 * 60, 8, &config);
    assert!(!series[0].unwrap().is_quiet(), "the busy minute is oldest");
    assert!(series[7].unwrap().is_quiet(), "as_of is the newest column");
}

#[test]
fn wide_columns_aggregate_consecutive_buckets() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    for minute in 0..16u64 {
        let state = if minute < 8 { "working" } else { "idle" };
        history.record(
            &one(T0 + minute * 60, "w1", "alpha", &[("w1:p1", state, 10)]),
            &config,
        );
    }

    let activity = &history.activity(T0 + 15 * 60, 2, 8, &config)[0];
    assert_eq!(activity.series.len(), 2);
    assert!(!activity.series[0].unwrap().is_quiet());
    assert!(activity.series[1].unwrap().is_quiet());
}

#[test]
fn a_window_reaching_before_the_epoch_does_not_underflow() {
    let config = config(60, 240, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(120, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    let series = series(&history, 120, 8, &config);
    assert_eq!(series.len(), 8);
    assert!(series[7].is_some());
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn recorded(config: &Config) -> History {
    let mut history = History::empty(config);
    for minute in 0..4u64 {
        history.record(
            &sample(
                T0 + minute * 60,
                vec![
                    workspace("wA", "alpha", &[("wA:p1", "working", 10 + minute)]),
                    workspace("wB", "beta", &[("wB:p1", "idle", 500)]),
                ],
            ),
            config,
        );
    }
    history
}

#[test]
fn a_saved_history_loads_back_identically() {
    let dir = TempDir::new("round-trip");
    let config = config(60, 16, 8);
    let history = recorded(&config);

    history::save_to(dir.path(), &history).expect("save");
    let loaded = history::load_from(dir.path(), &config);

    assert_eq!(loaded, history);
    assert_eq!(
        loaded.activity(T0 + 3 * 60, 8, 1, &config),
        history.activity(T0 + 3 * 60, 8, 1, &config)
    );
}

#[test]
fn a_missing_file_is_an_empty_history_and_not_an_error() {
    let dir = TempDir::new("missing");
    let config = config(60, 16, 8);

    let loaded = history::load_from(dir.path(), &config);

    assert_eq!(loaded, History::empty(&config));
}

#[test]
fn a_missing_state_directory_is_an_empty_history() {
    let dir = TempDir::new("absent");
    let config = config(60, 16, 8);
    let nowhere = dir.file("not-created-yet");

    assert_eq!(
        history::load_from(&nowhere, &config),
        History::empty(&config)
    );
}

#[test]
fn a_corrupt_file_is_discarded_rather_than_read() {
    let dir = TempDir::new("corrupt");
    let config = config(60, 16, 8);
    std::fs::write(dir.file("history.json"), b"{not json at all").unwrap();

    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn a_half_written_file_does_not_poison_the_next_run() {
    let dir = TempDir::new("truncated");
    let config = config(60, 16, 8);
    let history = recorded(&config);
    history::save_to(dir.path(), &history).expect("save");

    // Exactly what a crash mid-write would leave if the write were not atomic.
    let whole = std::fs::read(dir.file("history.json")).unwrap();
    std::fs::write(dir.file("history.json"), &whole[..whole.len() / 2]).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded, History::empty(&config));
    // And the sampler can carry straight on from the empty history.
    let mut loaded = loaded;
    loaded.record(&one(T0, "wA", "alpha", &[("wA:p1", "working", 1)]), &config);
    assert_eq!(loaded.workspaces.len(), 1);
}

#[test]
fn an_empty_file_is_discarded() {
    let dir = TempDir::new("empty-file");
    let config = config(60, 16, 8);
    std::fs::write(dir.file("history.json"), b"").unwrap();

    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn a_file_from_a_future_format_is_discarded() {
    let dir = TempDir::new("future-format");
    let config = config(60, 16, 8);
    let mut value = serde_json::to_value(recorded(&config)).unwrap();
    value["version"] = serde_json::json!(FORMAT_VERSION + 1);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn a_file_from_an_older_format_is_discarded() {
    let dir = TempDir::new("old-format");
    let config = config(60, 16, 8);
    let mut value = serde_json::to_value(recorded(&config)).unwrap();
    value["version"] = serde_json::json!(0);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn a_file_written_with_a_different_bucket_width_is_discarded() {
    let dir = TempDir::new("bucket-width");
    let written = config(60, 16, 8);
    history::save_to(dir.path(), &recorded(&written)).expect("save");

    // The same buckets mean something different at thirty seconds each, and
    // mixing two scales in one series would draw a plausible lie.
    let live = config(30, 16, 8);
    assert_eq!(history::load_from(dir.path(), &live), History::empty(&live));
}

#[test]
fn saving_leaves_no_temporary_file_behind() {
    let dir = TempDir::new("no-temp");
    let config = config(60, 16, 8);

    history::save_to(dir.path(), &recorded(&config)).expect("save");

    assert!(dir.file("history.json").exists());
    assert!(
        !dir.file("history.json.tmp").exists(),
        "the temp file is renamed, not left"
    );
}

#[test]
fn a_leftover_temporary_file_is_ignored_by_load() {
    let dir = TempDir::new("leftover-temp");
    let config = config(60, 16, 8);
    let history = recorded(&config);
    history::save_to(dir.path(), &history).expect("save");
    // What a crash between write and rename leaves behind.
    std::fs::write(dir.file("history.json.tmp"), b"half a file").unwrap();

    assert_eq!(history::load_from(dir.path(), &config), history);
}

#[test]
fn saving_twice_replaces_the_previous_file() {
    let dir = TempDir::new("replace");
    let config = config(60, 16, 8);
    history::save_to(dir.path(), &recorded(&config)).expect("first save");

    let mut later = recorded(&config);
    later.record(
        &one(T0 + 10 * 60, "wC", "gamma", &[("wC:p1", "working", 900)]),
        &config,
    );
    history::save_to(dir.path(), &later).expect("second save");

    let loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded, later);
    assert_eq!(loaded.workspaces.len(), 3);
}

#[test]
fn saving_creates_the_state_directory() {
    let dir = TempDir::new("create-dir");
    let nested = dir.file("state/pulse");
    let config = config(60, 16, 8);

    history::save_to(&nested, &recorded(&config)).expect("save into a fresh directory");

    assert!(nested.join("history.json").exists());
}

#[test]
fn saving_the_same_history_twice_produces_the_same_bytes() {
    // Byte stability is what makes a corrupt file obvious and a diff useful; it
    // also proves the per-agent seq table is ordered rather than incidental.
    let dir = TempDir::new("stable-bytes");
    let config = config(60, 16, 8);

    let mut forwards = History::empty(&config);
    forwards.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[("w1:p1", "working", 10), ("w1:p2", "idle", 11)],
        ),
        &config,
    );
    let mut backwards = History::empty(&config);
    backwards.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[("w1:p2", "idle", 11), ("w1:p1", "working", 10)],
        ),
        &config,
    );

    history::save_to(dir.path(), &forwards).expect("save");
    let first = std::fs::read(dir.file("history.json")).unwrap();
    history::save_to(dir.path(), &backwards).expect("save");
    let second = std::fs::read(dir.file("history.json")).unwrap();

    assert_eq!(first, second);
}

#[test]
fn a_restart_resumes_the_series_with_a_gap_for_the_downtime() {
    let dir = TempDir::new("restart");
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);
    for minute in 0..3u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }
    history::save_to(dir.path(), &history).expect("save");

    // Forty minutes of downtime, then the daemon comes back.
    let mut resumed = history::load_from(dir.path(), &config);
    resumed.record(
        &one(T0 + 43 * 60, "w1", "alpha", &[("w1:p1", "working", 60)]),
        &config,
    );

    let series = series(&resumed, T0 + 43 * 60, 44, &config);
    assert!(series[0].is_some(), "the pre-restart minutes survived");
    assert!(series[1].is_some());
    assert!(series[2].is_some());
    assert!(
        series[3..43].iter().all(Option::is_none),
        "forty minutes off is forty gaps, not forty quiet minutes"
    );
    assert!(series[43].is_some());
}

#[test]
fn a_ring_written_at_another_length_is_relaid_not_reindexed() {
    // Changing `retention_buckets` between runs changes what `number % len`
    // means. Truncating or extending the vector in place would keep every bucket
    // but point each at the wrong minute.
    let dir = TempDir::new("relaid");
    let written = config(60, 8, 8);
    let mut history = History::empty(&written);
    for minute in 0..6u64 {
        let state = if minute % 2 == 0 { "working" } else { "idle" };
        history.record(
            &one(T0 + minute * 60, "w1", "alpha", &[("w1:p1", state, 10)]),
            &written,
        );
    }
    let before = history.activity(T0 + 5 * 60, 6, 1, &written)[0]
        .series
        .clone();
    history::save_to(dir.path(), &history).expect("save");

    let widened = config(60, 32, 8);
    let loaded = history::load_from(dir.path(), &widened);
    assert_eq!(loaded.workspaces[0].buckets.len(), 32);
    assert_eq!(
        loaded.activity(T0 + 5 * 60, 6, 1, &widened)[0].series,
        before,
        "the same minutes must still read as the same minutes"
    );

    let narrowed = config(60, 4, 8);
    let loaded = history::load_from(dir.path(), &narrowed);
    assert_eq!(loaded.workspaces[0].buckets.len(), 4);
    assert_eq!(
        loaded.activity(T0 + 5 * 60, 4, 1, &narrowed)[0].series,
        before[2..],
        "narrowing keeps the newest minutes, in place"
    );
}

#[test]
fn a_file_with_more_workspaces_than_the_cap_is_evicted_on_load() {
    let dir = TempDir::new("cap-on-load");
    let generous = config(60, 16, 8);
    let mut history = History::empty(&generous);
    for index in 0..6u64 {
        history.record(
            &one(
                T0 + index * 60,
                &format!("w{index}"),
                &format!("workspace-{index}"),
                &[("p1", "idle", index)],
            ),
            &generous,
        );
    }
    history::save_to(dir.path(), &history).expect("save");

    let tight = config(60, 16, 2);
    let loaded = history::load_from(dir.path(), &tight);
    let ids: Vec<&str> = loaded
        .workspaces
        .iter()
        .map(|w| w.workspace_id.as_str())
        .collect();
    assert_eq!(ids, vec!["w4", "w5"], "the two most recently seen survive");
}

#[test]
fn duplicate_workspace_ids_in_a_file_are_reduced_to_one() {
    // A hand-edited or merged file could carry two entries for one id, which
    // would leave `record` updating one while the renderer drew both — one of
    // the two sparklines silently frozen.
    let dir = TempDir::new("duplicate-ids");
    let config = config(60, 16, 8);
    let mut value = serde_json::to_value(recorded(&config)).unwrap();
    let first = value["workspaces"][0].clone();
    value["workspaces"].as_array_mut().unwrap().push(first);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    assert_eq!(loaded.workspaces.len(), 2);
    assert_eq!(loaded.workspaces[0].workspace_id, "wA");
    assert_eq!(loaded.workspaces[1].workspace_id, "wB");
}

#[test]
fn forget_deletes_the_history_and_any_temporary_file() {
    let dir = TempDir::new("forget");
    let config = config(60, 16, 8);
    history::save_to(dir.path(), &recorded(&config)).expect("save");
    std::fs::write(dir.file("history.json.tmp"), b"leftover").unwrap();

    history::forget_in(dir.path()).expect("forget");

    assert!(!dir.file("history.json").exists());
    assert!(!dir.file("history.json.tmp").exists());
    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn forgetting_nothing_is_not_an_error() {
    let dir = TempDir::new("forget-missing");

    history::forget_in(dir.path()).expect("forgetting an absent history is the requested state");
    history::forget_in(&dir.file("never-existed")).expect("nor is an absent directory");
}

// ---------------------------------------------------------------------------
// Against the captured live snapshot
// ---------------------------------------------------------------------------

#[test]
fn the_live_snapshot_records_every_workspace() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    let sample = live_sample(T0);
    assert_eq!(sample.workspaces.len(), 10);

    history.record(&sample, &config);

    let activity = history.activity(T0, 8, 1, &config);
    assert_eq!(activity.len(), 10);
    let counts: Vec<(&str, usize)> = activity
        .iter()
        .map(|a| (a.workspace_id.as_str(), a.agent_count))
        .collect();
    assert_eq!(
        counts,
        vec![
            ("wM", 1),
            ("w3", 1),
            ("w6", 1),
            ("wE", 2),
            ("wY", 2),
            ("w15", 4),
            ("w16", 4),
            ("w1B", 1),
            ("w1C", 1),
            ("w1D", 1),
        ]
    );
    assert!(
        activity.iter().all(|a| a.series[7].is_some()),
        "every workspace in the snapshot was observed"
    );
    assert!(
        activity
            .iter()
            .all(|a| a.series[..7].iter().all(Option::is_none)),
        "and nothing before it was"
    );
}

#[test]
fn the_live_snapshot_aggregates_the_states_it_carries() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    history.record(&live_sample(T0), &config);

    let activity = history.activity(T0, 4, 1, &config);
    let state = |id: &str| {
        activity
            .iter()
            .find(|a| a.workspace_id == id)
            .unwrap()
            .state
    };
    assert_eq!(state("wM"), AgentState::Idle);
    assert_eq!(state("wY"), AgentState::Done);
    assert_eq!(state("w16"), AgentState::Working);
    // w15 has three working agents and one idle: working wins over idle.
    assert_eq!(state("w15"), AgentState::Working);
}

#[test]
fn one_blocked_agent_wins_the_live_workspace_it_is_in() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    let mut sample = live_sample(T0);
    // w16's four agents are all working in the capture; block one of them.
    let target = sample
        .workspaces
        .iter_mut()
        .find(|w| w.workspace_id == "w16")
        .unwrap();
    target.agents[2].state = AgentState::Blocked;
    history.record(&sample, &config);

    let activity = history.activity(T0, 4, 1, &config);
    let w16 = activity.iter().find(|a| a.workspace_id == "w16").unwrap();
    assert_eq!(activity.len(), 10);
    assert_eq!(w16.state, AgentState::Blocked);
    let index = history
        .workspaces
        .iter()
        .position(|w| w.workspace_id == "w16")
        .unwrap();
    let bucket = newest_bucket(&history, index);
    assert_eq!(bucket.blocked, 1);
    assert_eq!(bucket.working, 1, "three of them are still working");
}

#[test]
fn a_live_agent_that_moved_between_two_snapshots_is_a_transition() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    history.record(&live_sample(T0), &config);

    // The next snapshot five seconds later: w1C's agent still reads `working`,
    // but the session-global sequence moved it past the 796 that was the highest
    // in the capture. It finished a turn and started another.
    let mut second = live_sample(T0 + 5);
    let moved = second
        .workspaces
        .iter_mut()
        .find(|w| w.workspace_id == "w1C")
        .unwrap();
    assert_eq!(moved.agents[0].state, AgentState::Working);
    moved.agents[0].state_change_seq = 801;
    history.record(&second, &config);

    let transitions = |id: &str| {
        let index = history
            .workspaces
            .iter()
            .position(|w| w.workspace_id == id)
            .unwrap();
        newest_bucket(&history, index).transitions
    };
    assert_eq!(transitions("w1C"), 1);
    assert_eq!(
        transitions("w1D"),
        0,
        "nobody else moved, whatever the global counter did"
    );
}

#[test]
fn a_live_session_replayed_for_an_hour_stays_bounded_and_full() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    let base = live_sample(T0);

    // 720 snapshots at the default five-second interval.
    for step in 0..720u64 {
        let mut sample = base.clone();
        sample.taken_at = T0 + step * 5;
        for workspace in &mut sample.workspaces {
            for agent in &mut workspace.agents {
                // A transition roughly every minute per agent, as a busy session
                // produces.
                agent.state_change_seq += step / 12;
            }
        }
        history.record(&sample, &config);
    }

    assert_eq!(history.workspaces.len(), 10);
    assert!(history.encoded_len() < 512 * 1024);
    for workspace in &history.workspaces {
        assert_eq!(workspace.buckets.len(), 240);
    }
    let activity = history.activity(T0 + 719 * 5, 8, 8, &config);
    assert!(
        activity
            .iter()
            .all(|a| a.series.iter().all(Option::is_some)),
        "an hour of continuous sampling has no gaps"
    );
    let busy = activity.iter().find(|a| a.workspace_id == "w16").unwrap();
    assert!(busy.series.iter().all(|c| !c.unwrap().is_quiet()));
    let done = activity.iter().find(|a| a.workspace_id == "wY").unwrap();
    assert_eq!(done.state, AgentState::Done);
}

#[test]
fn a_live_workspace_that_closes_leaves_a_gap_not_a_quiet_stretch() {
    let config = config(60, 240, 64);
    let mut history = History::empty(&config);
    let base = live_sample(T0);
    let closing = live_workspace(&base, "w1B");

    for minute in 0..6u64 {
        let mut sample = base.clone();
        sample.taken_at = T0 + minute * 60;
        if (2..=4).contains(&minute) {
            sample
                .workspaces
                .retain(|w| w.workspace_id != closing.workspace_id);
        }
        history.record(&sample, &config);
    }

    let activity = history.activity(T0 + 5 * 60, 6, 1, &config);
    let closed = activity.iter().find(|a| a.workspace_id == "w1B").unwrap();
    assert!(closed.series[0].is_some());
    assert!(closed.series[1].is_some());
    assert_eq!(closed.series[2..5], [None, None, None]);
    assert!(closed.series[5].is_some());
}

// ---------------------------------------------------------------------------
// Hostile and misconfigured input
// ---------------------------------------------------------------------------

#[test]
fn a_file_recorded_by_a_clock_ahead_of_this_one_reads_as_gaps() {
    // The machine's clock was a day fast when the history was written, and has
    // since been corrected. Every recorded bucket is in this run's future, and
    // `number % len` would happily hand those slots back for today's minutes.
    let dir = TempDir::new("future-clock");
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);
    history.record(
        &one(
            T0 + 24 * 60 * 60,
            "w1",
            "alpha",
            &[("w1:p1", "working", 10)],
        ),
        &config,
    );
    history::save_to(dir.path(), &history).expect("save");

    let mut loaded = history::load_from(dir.path(), &config);
    let stale = series(&loaded, T0, 8, &config);
    assert!(
        stale.iter().all(Option::is_none),
        "tomorrow's buckets are not today's data: {stale:?}"
    );

    // Rendering gaps is only half of it. Asserting nothing more than that is how
    // this file's own permanent-freeze bug passed a green suite: the store must
    // also still be able to *record*, or every sample for the next twenty-four
    // hours is discarded and the anchor keeps the freeze across restarts.
    for minute in 0..4u64 {
        loaded.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 20 + minute)],
            ),
            &config,
        );
    }

    let resumed = series(&loaded, T0 + 3 * 60, 4, &config);
    assert!(
        resumed.iter().all(Option::is_some),
        "the corrected clock's samples must be recorded, not discarded: {resumed:?}"
    );
    let activity = &loaded.activity(T0 + 3 * 60, 4, 1, &config)[0];
    assert_eq!(
        activity.last_seen,
        Some(T0 + 3 * 60),
        "the fast clock's timestamp must not survive as the freshness stamp"
    );
    assert!(activity.is_current(T0 + 3 * 60, 15));
}

#[test]
fn a_bucket_claiming_more_working_samples_than_samples_is_clamped() {
    // Only reachable through a hand-edited or corrupt file, but an unclamped
    // occupancy would multiply out past the ramp and wrap into a small number —
    // a busy workspace drawn as a quiet one.
    let dir = TempDir::new("impossible-bucket");
    let config = config(60, 16, 8);
    let mut value = serde_json::to_value(recorded(&config)).unwrap();
    let overfull = serde_json::json!({
        "samples": 1,
        "working": 9_999,
        "blocked": 0,
        "transitions": 0,
    });
    let buckets = value["workspaces"][0]["buckets"].as_array().unwrap().len();
    value["workspaces"][0]["buckets"] = serde_json::json!(vec![overfull; buckets]);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    let level = series(&loaded, T0 + 3 * 60, 1, &config)[0].expect("observed");
    assert!(level.0 <= Level::MAX);
    assert_eq!(level, Level(6));
}

#[test]
fn a_directory_where_the_history_file_belongs_is_an_empty_history() {
    let dir = TempDir::new("file-is-a-directory");
    let config = config(60, 16, 8);
    std::fs::create_dir(dir.file("history.json")).unwrap();

    // An unreadable path is not a missing one, and must be reported rather than
    // passed off as a first run — but it still must not stop the sampler.
    assert_eq!(
        history::load_from(dir.path(), &config),
        History::empty(&config)
    );
}

#[test]
fn a_save_that_cannot_happen_reports_rather_than_pretending() {
    let dir = TempDir::new("unwritable");
    let config = config(60, 16, 8);
    std::fs::write(dir.file("in-the-way"), b"not a directory").unwrap();

    let failed = history::save_to(&dir.file("in-the-way/state"), &recorded(&config));

    assert!(failed.is_err(), "a save into a file is not a save");
}

#[test]
fn a_zero_bucket_width_does_not_divide_by_zero() {
    // `config::load` clamps this away; a hand-built config in another module's
    // test would not.
    let config = config(0, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    history.record(
        &one(T0 + 600, "w1", "alpha", &[("w1:p1", "idle", 11)]),
        &config,
    );

    assert_eq!(history.workspaces.len(), 1);
    assert_eq!(series(&history, T0 + 600, 4, &config).len(), 4);
}

#[test]
fn a_column_wider_than_the_whole_ring_still_reports_its_data() {
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    for minute in 0..4u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    let activity = &history.activity(T0 + 3 * 60, 1, 10_000, &config)[0];
    assert_eq!(activity.series.len(), 1);
    assert!(activity.series[0].is_some());
}

#[test]
fn an_agent_that_left_and_came_back_is_a_first_sighting_again() {
    // A deliberate limit, pinned here so it cannot change by accident. Seqs are
    // kept only for the agents present at the last observation, because keeping
    // them for departed panes is unbounded growth in a workspace that churns
    // panes. The cost is that a pane which vanished for one sample and returned
    // with a moved seq is not counted — one missed transition, against a size
    // bound that has to hold for weeks.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);

    history.record(
        &one(
            T0,
            "w1",
            "alpha",
            &[("w1:p1", "working", 10), ("w1:p2", "idle", 20)],
        ),
        &config,
    );
    history.record(
        &one(T0 + 5, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );
    history.record(
        &one(
            T0 + 10,
            "w1",
            "alpha",
            &[("w1:p1", "working", 10), ("w1:p2", "idle", 30)],
        ),
        &config,
    );

    assert_eq!(newest_bucket(&history, 0).transitions, 0);
    assert_eq!(newest_bucket(&history, 0).samples, 3);
}

#[test]
fn the_order_workspaces_arrive_in_does_not_change_their_series() {
    let config = config(60, 16, 8);
    let build = |reversed: bool| {
        let mut history = History::empty(&config);
        for minute in 0..4u64 {
            let mut workspaces = vec![
                workspace("wA", "alpha", &[("wA:p1", "working", 10 + minute)]),
                workspace("wB", "beta", &[("wB:p1", "idle", 500)]),
                workspace("wC", "gamma", &[("wC:p1", "blocked", 700 + minute)]),
            ];
            if reversed {
                workspaces.reverse();
            }
            history.record(&sample(T0 + minute * 60, workspaces), &config);
        }
        let mut activity = history.activity(T0 + 3 * 60, 8, 1, &config);
        activity.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
        activity
    };

    assert_eq!(build(false), build(true));
}

#[test]
fn a_forgotten_history_starts_a_new_series_rather_than_resuming() {
    let dir = TempDir::new("forget-then-record");
    let config = config(60, 16, 8);
    history::save_to(dir.path(), &recorded(&config)).expect("save");
    history::forget_in(dir.path()).expect("forget");

    let mut resumed = history::load_from(dir.path(), &config);
    assert!(resumed.workspaces.is_empty());
    resumed.record(
        &one(T0 + 60, "wA", "alpha", &[("wA:p1", "working", 10)]),
        &config,
    );

    let series = series(&resumed, T0 + 60, 4, &config);
    assert_eq!(series[..3], [None, None, None]);
    assert!(series[3].is_some());
}

// ---------------------------------------------------------------------------
// A clock that stepped forward and came back
// ---------------------------------------------------------------------------

#[test]
fn a_forward_clock_step_does_not_deafen_the_store() {
    // The shape the daemon actually produces: one snapshot stamped by a clock
    // that was ninety minutes fast — a machine booting on a fast RTC before
    // chronyd corrects it — and then an hour of correct once-a-minute sampling.
    // Every one of those correct samples used to be discarded, silently, because
    // the anchor the bogus sample left behind was never revised downward.
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);

    history.record(
        &one(T0 + 90 * 60, "w1", "alpha", &[("w1:p1", "working", 700)]),
        &config,
    );
    for minute in 0..60u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 701 + minute)],
            ),
            &config,
        );
    }

    let observed = history.workspaces[0]
        .buckets
        .iter()
        .filter(|bucket| bucket.observed())
        .count();
    assert_eq!(
        observed, 60,
        "every corrected sample must have been recorded"
    );
    let recent = series(&history, T0 + 59 * 60, 8, &config);
    assert!(
        recent.iter().all(Option::is_some),
        "the last eight minutes were sampled without interruption: {recent:?}"
    );
    assert_eq!(history.workspaces[0].newest_bucket, (T0 + 59 * 60) / 60);
}

#[test]
fn a_sample_far_behind_re_anchors_and_discards_the_unreachable_buckets() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for minute in 0..4u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    // Three buckets back: past the jitter window, so the anchor is the value
    // that cannot be believed.
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 20)]), &config);

    assert_eq!(history.workspaces[0].newest_bucket, T0 / 60);
    assert_eq!(
        newest_bucket(&history, 0).samples,
        2,
        "the re-anchored sample shares the minute it was recorded in"
    );
    let series = series(&history, T0, 4, &config);
    assert_eq!(
        series[..3],
        [None, None, None],
        "the buckets the stepped clock wrote are discarded, not left as data"
    );
    assert!(series[3].is_some());
}

#[test]
fn re_anchoring_forgets_a_state_duration_it_cannot_justify() {
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);
    // Two samples an hour into the future, the second a transition — so
    // `state_since` is stamped with a time that has not happened.
    history.record(
        &one(T0 + 60 * 60, "w1", "alpha", &[("w1:p1", "idle", 10)]),
        &config,
    );
    history.record(
        &one(T0 + 61 * 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    assert!(history.workspaces[0].state_since.is_some());

    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    let activity = &history.activity(T0, 4, 1, &config)[0];
    assert_eq!(activity.state, AgentState::Working);
    assert_eq!(
        activity.state_for, None,
        "the only stamp we had for this state was a time that never happened"
    );
    assert_eq!(activity.last_seen, Some(T0));
    assert_eq!(activity.observed_ago(T0), Some(0));
}

// ---------------------------------------------------------------------------
// Freshness: `state` is a past observation, and says how old it is
// ---------------------------------------------------------------------------

#[test]
fn a_state_nobody_has_watched_for_hours_is_not_reported_as_hours_of_work() {
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    history.record(
        &one(T0 + 120, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    // The daemon stops. Five hours later the user runs `pulse --once`.
    let as_of = T0 + 5 * 60 * 60;
    let activity = &history.activity(as_of, 8, 1, &config)[0];

    assert!(
        activity.series.iter().all(Option::is_none),
        "nothing was observed in the last eight minutes"
    );
    assert_eq!(
        activity.state_for,
        Some(60),
        "one minute of working was observed; the other five hours were not"
    );
    assert_eq!(activity.last_seen, Some(T0 + 120));
    assert_eq!(activity.observed_ago(as_of), Some(5 * 60 * 60 - 120));
    assert!(
        !activity.is_current(as_of, 15),
        "a five-hour-old observation is not a present-tense fact"
    );
    assert!(activity.is_current(T0 + 125, 15), "and a fresh one is");
}

#[test]
fn a_state_duration_does_not_grow_while_nobody_is_looking() {
    let config = config(60, 240, 8);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    history.record(
        &one(T0 + 120, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    let at_the_time = history.activity(T0 + 120, 8, 1, &config)[0].state_for;
    let much_later = history.activity(T0 + 9 * 60 * 60, 8, 1, &config)[0].state_for;

    assert_eq!(at_the_time, Some(60));
    assert_eq!(
        much_later, at_the_time,
        "an observed duration is a fact about the past and cannot grow"
    );
}

#[test]
fn a_workspace_that_was_never_observed_reports_no_last_seen() {
    // Unreachable from `record`, which always stamps the observation that
    // created the workspace — but a hand-edited file can carry a zero, and
    // reporting that as a 1970 sighting would make every freshness check read
    // "observed, just very long ago" rather than "never observed".
    let dir = TempDir::new("never-observed");
    let config = config(60, 16, 8);
    let mut value = serde_json::to_value(recorded(&config)).unwrap();
    value["workspaces"][0]["last_seen"] = serde_json::json!(0);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    let loaded = history::load_from(dir.path(), &config);
    let activity = &loaded.activity(T0 + 3 * 60, 8, 1, &config)[0];
    assert_eq!(activity.last_seen, None);
    assert_eq!(activity.observed_ago(T0 + 3 * 60), None);
    assert!(!activity.is_current(T0 + 3 * 60, 15));
}

// ---------------------------------------------------------------------------
// Column geometry against the live lap
// ---------------------------------------------------------------------------

#[test]
fn a_column_wider_than_the_ring_still_finds_the_live_lap() {
    // Eight buckets of a ring that holds eight, all observed and busy, drawn in
    // one twenty-bucket column. Clamping the walk a ring length back from the
    // *column's* newest bucket starts it past every live slot the moment `as_of`
    // runs ahead of the last observation.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for minute in 0..8u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    for ahead in [0u64, 1, 8, 13] {
        let as_of = T0 + (7 + ahead) * 60;
        let column = history.activity(as_of, 1, 20, &config)[0].series[0];
        assert!(
            column.is_some(),
            "eight busy minutes are inside this column, {ahead} buckets after the last one"
        );
    }
}

#[test]
fn the_badge_geometry_never_asks_for_more_than_the_ring_holds() {
    // The cross-module invariant that keeps the badge off the wide-column path
    // above: `--bucket-seconds 10 --columns 1` derives 384 buckets per column
    // from the 64-minute window, which `config` now clamps down to what the ring
    // can afford. Pinned here because the two halves live in different modules
    // and neither one alone shows the mismatch.
    for bucket_seconds in [10u64, 30, 60, 3_600] {
        for badge_columns in [1usize, 8, 64] {
            let config = Config {
                bucket_seconds,
                badge_columns,
                ..Config::default()
            };
            let per_column = config.buckets_per_badge_column();
            assert!(
                per_column * badge_columns <= config.retention_buckets,
                "a {badge_columns}-column badge at {bucket_seconds}s asks for \
                 {per_column} buckets per column against a {}-bucket ring",
                config.retention_buckets
            );
        }
    }

    // And with that geometry the store's answer for a fully sampled window has
    // no gaps in it.
    let config = Config {
        bucket_seconds: 10,
        badge_columns: 1,
        ..Config::default()
    };
    let per_column = config.buckets_per_badge_column();
    let mut history = History::empty(&config);
    for step in 0..240u64 {
        history.record(
            &one(
                T0 + step * 10,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + step)],
            ),
            &config,
        );
    }

    let badge = history.activity(T0 + 239 * 10, config.badge_columns, per_column, &config);
    assert!(badge[0].series.iter().all(Option::is_some));
}

#[test]
fn a_wide_column_entirely_past_the_live_lap_is_still_a_gap() {
    // The counterweight to the test above: clamping the walk onto the live lap
    // must intersect the column with it, never replace it. A column covering
    // only minutes after the last observation contains no observation.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for minute in 0..8u64 {
        history.record(
            &one(
                T0 + minute * 60,
                "w1",
                "alpha",
                &[("w1:p1", "working", 10 + minute)],
            ),
            &config,
        );
    }

    // Two columns twenty buckets wide, ending forty buckets past the newest
    // recorded one: both cover only unobserved minutes.
    let as_of = T0 + (7 + 40) * 60;
    let series = history.activity(as_of, 2, 20, &config)[0].series.clone();
    assert_eq!(series, vec![None, None], "{series:?}");
}

#[test]
fn an_absurdly_distant_as_of_neither_hangs_nor_invents_data() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(
        &one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]),
        &config,
    );

    // The walk is bounded by the ring, not by the distance to `as_of`.
    let series = history.activity(u64::MAX / 2, 8, 10_000, &config)[0]
        .series
        .clone();
    assert!(series.iter().all(Option::is_none), "{series:?}");
}

#[test]
fn a_history_file_with_an_impossible_anchor_does_not_panic() {
    // `newest_bucket` comes straight out of the file, and `number + len`
    // overflows for one near u64::MAX — a panic on the `pulse --once` path,
    // which the module header promises can never happen for a corrupt file.
    let dir = TempDir::new("impossible-anchor");
    let written = config(60, 16, 8);
    let relaid = config(60, 32, 8);
    let mut value = serde_json::to_value(recorded(&written)).unwrap();
    value["workspaces"][0]["newest_bucket"] = serde_json::json!(u64::MAX);
    std::fs::write(dir.file("history.json"), value.to_string()).unwrap();

    // Same ring length: `reshape` is a no-op and the overflow is reached through
    // the projection.
    let loaded = history::load_from(dir.path(), &written);
    let projected = series(&loaded, T0 + 3 * 60, 8, &written);
    assert!(projected.iter().all(Option::is_none), "{projected:?}");

    // A different ring length: reached through `reshape` on load instead.
    let loaded = history::load_from(dir.path(), &relaid);
    assert_eq!(loaded.workspaces[0].buckets.len(), 32);
    assert!(series(&loaded, T0 + 3 * 60, 8, &relaid)
        .iter()
        .all(Option::is_none));
}
