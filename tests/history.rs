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
use pulse::model::{AgentObservation, AgentState, Level, Sample, WorkspaceObservation};

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

/// A sample of a single workspace, which is what most of these tests need.
fn one(taken_at: u64, id: &str, label: &str, agents: &[(&str, &str, u64)]) -> Sample {
    Sample {
        taken_at,
        workspaces: vec![workspace(id, label, agents)],
    }
}

fn sample(taken_at: u64, workspaces: Vec<WorkspaceObservation>) -> Sample {
    Sample {
        taken_at,
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
    history.activity(as_of, columns, 1, config)[0].series.clone()
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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);
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
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

    assert!(history.activity(T0, 0, 1, &config)[0].series.is_empty());
}

#[test]
fn zero_buckets_per_column_behaves_as_one() {
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);
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
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

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
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

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
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);
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
fn a_sample_older_than_the_newest_bucket_is_dropped_not_written() {
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
    // The clock steps back two minutes: an NTP correction, a resumed laptop.
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "idle", 99)]),
        &config,
    );

    assert_eq!(
        history, before,
        "a backwards sample must change nothing at all"
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
    // busy minute from the previous lap.
    let jumped = T0 + 7 * 60;
    history.record(&one(jumped, "w1", "alpha", &[("w1:p1", "idle", 20)]), &config);

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
        history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", step)]), &config);
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

    history.record(&one(T0 + 60, "wC", "gamma", &[("wC:p1", "idle", 3)]), &config);

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
    history.record(&one(T0 + 60, "wB", "beta", &[("wB:p1", "working", 2)]), &config);
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
    history.record(&one(T0 + 60, "wA", "alpha", &[("wA:p1", "idle", 2)]), &config);

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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]), &config);
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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 700)]), &config);
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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]), &config);
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

    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 795)]), &config);
    history.record(&one(T0 + 5, "w1", "alpha", &[("w1:p1", "working", 3)]), &config);

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

    history.record(
        &one(T0 + 120, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );
    let activity = &history.activity(T0 + 180, 4, 1, &config)[0];
    assert_eq!(activity.state, AgentState::Working);
    assert_eq!(activity.state_for, Some(60));
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
    let config = config(60, 16, 4);
    let mut history = History::empty(&config);
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "idle", 10)]), &config);
    history.record(
        &one(T0 + 60, "w1", "alpha", &[("w1:p1", "working", 11)]),
        &config,
    );

    // Rendering with an `as_of` behind the transition would underflow into a
    // duration of several hundred billion years.
    assert_eq!(history.activity(T0, 4, 1, &config)[0].state_for, Some(0));
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
    assert_eq!(previous, Level(6), "a fully busy bucket leaves churn headroom");
}

#[test]
fn churn_lifts_a_bucket_that_never_caught_an_agent_working() {
    // Twelve idle samples with the seq moving under them: the agent has been
    // finishing turns between samples, which is not a quiet minute.
    let config = config(60, 8, 4);
    let mut history = History::empty(&config);
    for step in 0..12u64 {
        history.record(
            &one(T0 + step * 5, "w1", "alpha", &[("w1:p1", "idle", 10 + step)]),
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
    history.record(&one(T0, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);
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
    history.record(&one(120, "w1", "alpha", &[("w1:p1", "working", 10)]), &config);

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
        activity.iter().all(|a| a.series[..7].iter().all(Option::is_none)),
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
        activity.iter().all(|a| a.series.iter().all(Option::is_some)),
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
        if minute >= 2 && minute <= 4 {
            sample.workspaces.retain(|w| w.workspace_id != closing.workspace_id);
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
