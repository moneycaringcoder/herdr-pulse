//! Tests for the presenter.
//!
//! Everything here builds `WorkspaceActivity` values by hand. The renderer is a
//! pure function of them, so none of these tests need a store, a socket or a
//! daemon — which is also why they can run while `history` and `daemon` are
//! still unimplemented.
//!
//! The recurring theme is that a gap and a quiet bucket must never be confused,
//! and that columns line up when measured the way a terminal measures them.

use std::time::Duration;

use pulse::config::{Config, WEEK_BUCKET_SECONDS, WEEK_COLUMNS};
use pulse::daemon::{SamplerStop, StopReason};
use pulse::model::{AgentActivity, AgentState, Level, SessionMark, WorkspaceActivity};
use pulse::render::{
    badge, display_width, duration, json_document, pane, pane_geometry, sampler_stop_message,
    sparkline, staleness_tolerance, state_glyph, week_pane, SamplerState, GAP, QUIET, RAMP,
    TRANSITION_MARKER,
};

/// The `as_of` every pane test renders at. Fixed so a row's freshness is a
/// property of the test rather than of the wall clock.
const AS_OF: u64 = 1_723_000_000;

/// A workspace observed at [`AS_OF`], i.e. one whose state is current.
fn activity(
    label: &str,
    series: Vec<Option<Level>>,
    state: AgentState,
    state_for: Option<u64>,
    agent_count: usize,
) -> WorkspaceActivity {
    let transitions = series
        .iter()
        .map(|level| level.as_ref().map(|_| 0))
        .collect();
    WorkspaceActivity {
        workspace_id: format!("w-{label}"),
        label: label.to_string(),
        blocked_seconds: 0,
        watched_seconds: 1,
        series,
        state,
        state_for,
        last_seen: Some(AS_OF),
        agent_count,
        session: None,
        session_began: None,
        // A week of gaps unless a test says otherwise, with no watching behind
        // it: the pane tests are about the fine series, and an unset coarse ring
        // must not change what they see.
        week: vec![None; pulse::config::WEEK_COLUMNS],
        week_transitions: vec![None; pulse::config::WEEK_COLUMNS],
        week_blocked_seconds: 0,
        week_watched_seconds: 0,
        // No per-agent rings unless a test asks: the sampler only records them
        // when the user turns them on.
        agents: Vec::new(),
        // Observed columns with nothing moving in them, so a test that says
        // nothing about transitions gets no markers rather than an accidental
        // one. `with_transitions` sets them where a test means to.
        transitions,
    }
}

fn agent(
    pane_id: &str,
    program: Option<&str>,
    series: Vec<Option<Level>>,
    state: AgentState,
    seen_ago: u64,
    blocked_seconds: u64,
    watched_seconds: u64,
) -> AgentActivity {
    let transitions = series
        .iter()
        .map(|level| level.as_ref().map(|_| 0))
        .collect();
    AgentActivity {
        pane_id: pane_id.to_string(),
        program: program.map(str::to_string),
        state,
        transitions,
        series,
        last_seen: AS_OF - seen_ago,
        blocked_seconds,
        watched_seconds,
    }
}

/// Marks transitions on a workspace row, column for column with its series.
fn with_transitions(
    activity: WorkspaceActivity,
    transitions: Vec<Option<u32>>,
) -> WorkspaceActivity {
    WorkspaceActivity {
        transitions,
        ..activity
    }
}

/// Marks transitions on the week row, column for column with its series.
fn with_week_transitions(
    activity: WorkspaceActivity,
    transitions: Vec<Option<u32>>,
) -> WorkspaceActivity {
    WorkspaceActivity {
        week_transitions: transitions,
        ..activity
    }
}

/// Marks transitions on an individual agent row.
fn with_agent_transitions(agent: AgentActivity, transitions: Vec<Option<u32>>) -> AgentActivity {
    AgentActivity {
        transitions,
        ..agent
    }
}

fn with_agents(activity: WorkspaceActivity, agents: Vec<AgentActivity>) -> WorkspaceActivity {
    WorkspaceActivity { agents, ..activity }
}

/// Sets the blocked estimate and the watched time that supports it.
fn with_blocked_time(
    activity: WorkspaceActivity,
    blocked_seconds: u64,
    watched_seconds: u64,
) -> WorkspaceActivity {
    WorkspaceActivity {
        blocked_seconds,
        watched_seconds,
        ..activity
    }
}

fn session(fingerprint: &str, began: u64) -> SessionMark {
    SessionMark {
        fingerprint: fingerprint.to_string(),
        began,
    }
}

/// A sampler that is live: nothing to explain.
fn running() -> SamplerState<'static> {
    SamplerState {
        running: true,
        stop: None,
    }
}

/// A machine where no sampler was ever started: not running, and no stop to
/// report. The two are separate facts, and this is the pair that catches a
/// document deriving one from the other.
fn never_ran() -> SamplerState<'static> {
    SamplerState {
        running: false,
        stop: None,
    }
}

fn stopped(stop: &SamplerStop) -> SamplerState<'_> {
    SamplerState {
        running: false,
        stop: Some(stop),
    }
}

fn recorded_by(activity: WorkspaceActivity, session: &SessionMark) -> WorkspaceActivity {
    WorkspaceActivity {
        session: Some(session.fingerprint.clone()),
        session_began: Some(session.began),
        ..activity
    }
}

/// The same workspace, last observed `ago` seconds before [`AS_OF`].
fn seen_ago(activity: WorkspaceActivity, ago: u64) -> WorkspaceActivity {
    WorkspaceActivity {
        last_seen: Some(AS_OF - ago),
        ..activity
    }
}

/// A workspace that has never been observed at all.
fn never_seen(activity: WorkspaceActivity) -> WorkspaceActivity {
    WorkspaceActivity {
        last_seen: None,
        ..activity
    }
}

/// The row a workspace occupies in a rendered pane, found by its label.
fn row_for<'a>(rendered: &'a str, label: &str) -> &'a str {
    rendered
        .lines()
        .find(|line| line.starts_with(label))
        .unwrap_or_else(|| panic!("no row for {label:?} in\n{rendered}"))
}

fn agent_row_for<'a>(rendered: &'a str, label: &str) -> &'a str {
    rendered
        .lines()
        .find(|line| {
            line.strip_prefix("  ")
                .is_some_and(|line| line.starts_with(label))
        })
        .unwrap_or_else(|| panic!("no indented row for {label:?} in\n{rendered}"))
}

fn line_after<'a>(rendered: &'a str, row: &str) -> Option<&'a str> {
    let start = rendered.find(row)?.checked_add(row.len())?;
    rendered[start..].strip_prefix('\n')?.lines().next()
}

fn marked_columns(row: &str, marker_line: &str) -> Vec<usize> {
    let bracket = row
        .rfind('[')
        .unwrap_or_else(|| panic!("no sparkline in {row:?}"));
    let first_column = display_width(&row[..bracket]) + 1;
    marker_line
        .match_indices(TRANSITION_MARKER)
        .map(|(at, _)| display_width(&marker_line[..at]) - first_column)
        .collect()
}

fn levels(raw: &[u8]) -> Vec<Option<Level>> {
    raw.iter().map(|&n| Some(Level(n))).collect()
}

fn config_with_columns(badge_columns: usize) -> Config {
    Config {
        badge_columns,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// sparkline
// ---------------------------------------------------------------------------

#[test]
fn an_empty_series_renders_nothing() {
    assert_eq!(sparkline(&[]), "");
}

#[test]
fn a_single_column_renders_a_single_glyph() {
    assert_eq!(sparkline(&levels(&[0])), QUIET.to_string());
    assert_eq!(sparkline(&levels(&[4])), "▄");
    assert_eq!(sparkline(&[None]), GAP.to_string());
}

#[test]
fn every_level_maps_to_its_step_of_the_ramp() {
    for (index, expected) in RAMP.iter().enumerate() {
        let level = (index + 1) as u8;
        assert_eq!(
            sparkline(&levels(&[level])),
            expected.to_string(),
            "level {level}"
        );
    }
}

#[test]
fn an_all_zero_series_is_quiet_not_a_ramp_step() {
    let rendered = sparkline(&levels(&[0, 0, 0, 0, 0]));
    assert_eq!(rendered, QUIET.to_string().repeat(5));
    // The lowest ramp step reads as "a little activity" and must not be reused
    // for none at all.
    assert!(!rendered.contains(RAMP[0]));
}

#[test]
fn an_all_max_series_is_solid_blocks() {
    assert_eq!(sparkline(&levels(&[8, 8, 8])), "███");
}

#[test]
fn a_level_above_the_maximum_clamps_instead_of_indexing_out_of_bounds() {
    // `Level` is a public tuple struct, so nothing forces a value through
    // `Level::new`. Anything above the ramp saturates at its top step.
    assert_eq!(sparkline(&levels(&[9])), "█");
    assert_eq!(sparkline(&levels(&[100])), "█");
    assert_eq!(sparkline(&levels(&[u8::MAX])), "█");
}

#[test]
fn no_level_at_all_can_make_the_sparkline_panic() {
    for raw in 0..=u8::MAX {
        let rendered = sparkline(&levels(&[raw]));
        assert_eq!(
            rendered.chars().count(),
            1,
            "level {raw} rendered {rendered:?}"
        );
    }
}

#[test]
fn the_output_is_exactly_one_character_per_column() {
    let series = vec![
        Some(Level(0)),
        None,
        Some(Level(8)),
        None,
        Some(Level(3)),
        Some(Level(255)),
    ];
    // Characters, not bytes: the ramp is three bytes per glyph.
    assert_eq!(sparkline(&series).chars().count(), series.len());
    assert!(sparkline(&series).len() > series.len());
}

#[test]
fn a_series_longer_than_the_badge_still_renders_in_full() {
    // `sparkline` itself never truncates; windowing is the badge's job.
    let series: Vec<Option<Level>> = (0..500).map(|n| Some(Level((n % 9) as u8))).collect();
    assert_eq!(sparkline(&series).chars().count(), 500);
}

#[test]
fn a_series_that_is_entirely_gaps_never_renders_a_quiet_bucket() {
    let rendered = sparkline(&[None; 12]);
    assert_eq!(rendered, GAP.to_string().repeat(12));
    assert!(
        !rendered.contains(QUIET),
        "unobserved time must not be drawn as observed silence"
    );
    assert!(RAMP.iter().all(|step| !rendered.contains(*step)));
}

#[test]
fn a_gap_and_a_quiet_bucket_render_differently() {
    // The single most important property in this module.
    assert_ne!(GAP, QUIET);
    assert_ne!(sparkline(&[None]), sparkline(&levels(&[0])));
}

#[test]
fn gaps_interleaved_with_data_keep_their_positions() {
    let series = vec![
        Some(Level(8)),
        None,
        Some(Level(0)),
        None,
        None,
        Some(Level(1)),
    ];
    assert_eq!(sparkline(&series), format!("█{GAP}{QUIET}{GAP}{GAP}▁"));
}

// ---------------------------------------------------------------------------
// duration
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_duration_says_so() {
    // Not "0s". We have not observed a transition, which is not the same as
    // having observed one a moment ago.
    assert_eq!(duration(None), "?");
}

#[test]
fn seconds_minutes_hours_and_days_each_have_a_form() {
    assert_eq!(duration(Some(0)), "0s");
    assert_eq!(duration(Some(1)), "1s");
    assert_eq!(duration(Some(12)), "12s");
    assert_eq!(duration(Some(4 * 60)), "4m");
    assert_eq!(duration(Some(4_800)), "1h20");
    assert_eq!(duration(Some(3 * 86_400)), "3d");
}

#[test]
fn every_unit_boundary_flips_exactly_where_it_should() {
    assert_eq!(duration(Some(59)), "59s");
    assert_eq!(duration(Some(60)), "1m");
    assert_eq!(duration(Some(119)), "1m");
    assert_eq!(duration(Some(3_599)), "59m");
    assert_eq!(duration(Some(3_600)), "1h00");
    assert_eq!(duration(Some(3_660)), "1h01");
    assert_eq!(duration(Some(35_999)), "9h59");
    // Two-digit hours spend the four-character budget on their own.
    assert_eq!(duration(Some(36_000)), "10h");
    assert_eq!(duration(Some(86_399)), "23h");
    assert_eq!(duration(Some(86_400)), "1d");
    assert_eq!(duration(Some(99 * 86_400)), "99d");
}

#[test]
fn an_absurd_duration_saturates_instead_of_overflowing() {
    // A clock that jumped, or a `state_since` of zero against a real `as_of`.
    assert_eq!(duration(Some(100 * 86_400)), ">99d");
    assert_eq!(duration(Some(u64::MAX)), ">99d");
    assert_eq!(duration(Some(u64::MAX / 2)), ">99d");
}

#[test]
fn no_duration_ever_exceeds_four_columns() {
    let mut samples: Vec<u64> = (0..100_000).collect();
    samples.extend([u64::MAX, u64::MAX - 1, u64::MAX / 3, 1 << 40, 1 << 60]);
    for seconds in samples {
        let rendered = duration(Some(seconds));
        assert!(
            display_width(&rendered) <= 4,
            "{seconds}s rendered as {rendered:?}"
        );
    }
    assert!(display_width(&duration(None)) <= 4);
}

// ---------------------------------------------------------------------------
// sampler stop explanation
// ---------------------------------------------------------------------------

#[test]
fn a_disabled_sampler_names_the_choice_and_when_it_was_made() {
    let stop = SamplerStop {
        reason: StopReason::Disabled,
        at: Some(AS_OF - 14 * 60),
        detail: None,
    };

    assert_eq!(
        sampler_stop_message(Some(&stop), AS_OF).as_deref(),
        Some("no sampler is running — disabled 14m ago; nothing since then is recorded.")
    );
}

#[test]
fn a_terminated_sampler_names_the_termination_with_or_without_a_time() {
    let known = SamplerStop {
        reason: StopReason::Terminated,
        at: Some(AS_OF - 3 * 60),
        detail: None,
    };
    let unknown = SamplerStop {
        at: None,
        ..known.clone()
    };

    assert_eq!(
        sampler_stop_message(Some(&known), AS_OF).as_deref(),
        Some("no sampler is running — terminated 3m ago; nothing since then is recorded.")
    );
    assert_eq!(
        sampler_stop_message(Some(&unknown), AS_OF).as_deref(),
        Some("no sampler is running — terminated; nothing since then is recorded.")
    );
}

#[test]
fn a_failed_sampler_carries_the_failure_detail() {
    let stop = SamplerStop {
        reason: StopReason::Failed,
        at: Some(AS_OF - 3 * 60),
        detail: Some("history write failed".to_string()),
    };

    assert_eq!(
        sampler_stop_message(Some(&stop), AS_OF).as_deref(),
        Some(
            "no sampler is running — the last run ended unexpectedly 3m ago (history write failed); nothing since then is recorded."
        )
    );
}

#[test]
fn an_unannounced_stop_is_never_presented_as_clean() {
    let stop = SamplerStop {
        reason: StopReason::Unknown,
        at: None,
        detail: None,
    };

    assert_eq!(
        sampler_stop_message(Some(&stop), AS_OF).as_deref(),
        Some(
            "no sampler is running — the last run stopped for an unknown reason; nothing since then is recorded."
        )
    );
}

#[test]
fn a_live_sampler_has_nothing_to_explain() {
    assert_eq!(sampler_stop_message(None, AS_OF), None);
}

// ---------------------------------------------------------------------------
// state_glyph
// ---------------------------------------------------------------------------

#[test]
fn every_state_has_its_own_single_width_glyph() {
    let mut seen = Vec::new();
    for state in AgentState::ALL {
        let glyph = state_glyph(state);
        assert!(
            !seen.contains(&glyph),
            "{state} reuses the glyph {glyph:?}, so two states look alike"
        );
        assert_eq!(display_width(&glyph.to_string()), 1);
        assert!(!glyph.is_control());
        seen.push(glyph);
    }
    assert_eq!(seen.len(), AgentState::ALL.len());
}

#[test]
fn a_state_glyph_is_never_confused_with_a_sparkline_glyph() {
    // The badge concatenates the two with no separator, so they have to be
    // tellable apart by eye.
    for state in AgentState::ALL {
        let glyph = state_glyph(state);
        assert_ne!(glyph, QUIET);
        assert_ne!(glyph, GAP);
        assert!(!RAMP.contains(&glyph));
    }
}

// ---------------------------------------------------------------------------
// badge
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_with_no_recorded_history_gets_no_badge() {
    let config = Config::default();
    // Empty series and an all-gap series both mean "we have never recorded an
    // observation here". The daemon reads the empty string as "clear the token"
    // rather than drawing a row of blanks.
    assert_eq!(
        badge(
            &activity("new", vec![], AgentState::Working, None, 1),
            &config
        ),
        ""
    );
    assert_eq!(
        badge(
            &activity("new", vec![None; 8], AgentState::Blocked, Some(9), 2),
            &config
        ),
        ""
    );
}

#[test]
fn a_quiet_but_observed_workspace_still_gets_a_badge() {
    // The whole point of the plugin: "we watched, and nothing happened" is
    // information, and must not be mistaken for "we have nothing".
    //
    // Series length matches `badge_columns`, which is the only shape
    // `daemon::cycle` ever builds — it asks the store for exactly that many
    // columns.
    let config = config_with_columns(4);
    let rendered = badge(
        &activity(
            "resting",
            levels(&[0, 0, 0, 0]),
            AgentState::Idle,
            Some(600),
            1,
        ),
        &config,
    );
    assert_eq!(rendered, format!("{}{}", QUIET.to_string().repeat(4), '-'));
}

#[test]
fn the_badge_is_a_sparkline_followed_by_the_state_glyph() {
    let config = config_with_columns(4);
    let rendered = badge(
        &activity(
            "busy",
            levels(&[0, 2, 5, 8]),
            AgentState::Blocked,
            Some(30),
            3,
        ),
        &config,
    );
    assert_eq!(rendered, format!("{QUIET}▂▅█!"));
}

#[test]
fn a_series_with_no_observations_at_all_clears_the_badge() {
    // The actual clearing rule, at the shape the store produces: one column per
    // configured badge column, none of them observed. The daemon reads the empty
    // string as "clear the token".
    //
    // This is a floor rather than a routine path — the store only builds a series
    // for a workspace it has observed at least once, and a live sampler observes
    // every workspace every cycle — but `daemon::badge_plan` depends on it, so it
    // is pinned here.
    for columns in [1usize, 8, 64] {
        let config = config_with_columns(columns);
        let rendered = badge(
            &activity(
                "never",
                vec![None; columns],
                AgentState::Blocked,
                Some(4_000),
                1,
            ),
            &config,
        );
        assert!(
            rendered.is_empty(),
            "{columns} unobserved columns drew {rendered:?} instead of clearing"
        );
    }
}

#[test]
fn a_badge_is_one_glyph_per_column_the_store_was_asked_for() {
    // `daemon::cycle` asks for `config.badge_columns` columns, so this is the
    // production shape: the series *is* the window, and the badge draws all of it
    // plus the state glyph.
    for columns in [1usize, 2, 8, 16, 64] {
        let config = config_with_columns(columns);
        let series: Vec<Option<Level>> = levels(&vec![4u8; columns]);
        let rendered = badge(
            &activity("wide", series, AgentState::Idle, Some(1), 1),
            &config,
        );
        assert_eq!(
            rendered.chars().count(),
            columns + 1,
            "{columns} columns plus one state glyph"
        );
        assert_eq!(display_width(&rendered), columns + 1);
    }
}

#[test]
fn a_badge_never_exceeds_the_configured_sidebar_budget() {
    // A bounds guard on a public function, not a claim about the daemon. Nothing
    // in this crate hands `badge` a series longer than `badge_columns` — that is
    // exactly why the earlier "the badge windows a longer series" tests proved
    // nothing — but the width invariant belongs to this function rather than to a
    // convention between two modules, so a caller that got it wrong must still
    // not overflow the sidebar cell.
    for columns in [1usize, 3, 8] {
        let config = config_with_columns(columns);
        let series: Vec<Option<Level>> = levels(&vec![8u8; columns * 4]);
        let rendered = badge(
            &activity("over", series, AgentState::Working, Some(5), 1),
            &config,
        );
        assert_eq!(display_width(&rendered), columns + 1);
        // Newest columns kept: the badge answers "recently", not "ever".
        assert_eq!(rendered, format!("{}>", "█".repeat(columns)));
    }
}

#[test]
fn the_badge_fits_its_sidebar_budget_at_the_default_configuration() {
    // Eight columns is `DEFAULT_BADGE_COLUMNS`, so this is the default-config
    // production shape.
    let config = Config::default();
    let rendered = badge(
        &activity(
            "sized",
            levels(&[1, 2, 3, 4, 5, 6, 7, 8]),
            AgentState::Working,
            Some(60),
            2,
        ),
        &config,
    );
    // Eight columns plus a glyph, and no glyph wider than one cell.
    assert_eq!(display_width(&rendered), 9);
}

#[test]
fn the_badge_never_panics_whatever_the_series() {
    // Robustness, not semantics: the shapes here are deliberately wider than
    // anything the store builds, because the point is that no input can panic —
    // not that any particular one occurs.
    for columns in [1usize, 3, 8, 64] {
        let config = config_with_columns(columns);
        for length in [0usize, 1, 7, 8, 9, 300] {
            let series: Vec<Option<Level>> = (0..length)
                .map(|n| {
                    if n % 5 == 0 {
                        None
                    } else {
                        Some(Level((n % 300) as u8))
                    }
                })
                .collect();
            for state in AgentState::ALL {
                let _ = badge(&activity("fuzz", series.clone(), state, None, 0), &config);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// display width
// ---------------------------------------------------------------------------

#[test]
fn width_is_terminal_cells_not_bytes_or_code_points() {
    assert_eq!(display_width("abc"), 3);
    // Three bytes, one cell.
    assert_eq!("█".len(), 3);
    assert_eq!(display_width("█"), 1);
    // Two cells, one code point.
    assert_eq!(display_width("日"), 2);
    assert_eq!(display_width("日本語"), 6);
    // A combining accent rides on the previous cell.
    assert_eq!(display_width("e\u{0301}"), 1);
    assert_eq!(display_width("🔥"), 2);
    assert_eq!(display_width(""), 0);
}

// ---------------------------------------------------------------------------
// pane
// ---------------------------------------------------------------------------

/// Display width of each line up to `marker`, for the rows that have one.
///
/// Searches from the right: a workspace label is arbitrary user text and may
/// itself contain a bracket, but nothing to the right of the sparkline does.
fn column_offsets(rendered: &str, marker: char) -> Vec<usize> {
    rendered
        .lines()
        .filter_map(|line| line.rfind(marker).map(|at| display_width(&line[..at])))
        .collect()
}

fn sample_pane(rows: Vec<WorkspaceActivity>) -> String {
    pane(&rows, &Config::default(), AS_OF, None)
}

#[test]
fn a_live_session_and_an_earlier_session_are_labelled_differently() {
    let live = session("live-fingerprint", 13 * 3_600 + 28 * 60 + 59);
    let earlier = session("earlier-fingerprint", 3 * 3_600 + 7 * 60 + 42);
    let rendered = pane(
        &[
            recorded_by(
                activity("current", levels(&[5]), AgentState::Working, Some(4), 1),
                &live,
            ),
            recorded_by(
                activity("earlier", levels(&[8]), AgentState::Working, Some(4), 1),
                &earlier,
            ),
        ],
        &Config::default(),
        AS_OF,
        Some(&live),
    );

    let heading: Vec<&str> = rendered
        .lines()
        .find(|line| line.starts_with("workspace"))
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    assert_eq!(
        heading,
        [
            "workspace",
            "activity",
            "session",
            "state",
            "blocked",
            "for",
            "seen",
            "agents"
        ]
    );

    let current: Vec<&str> = row_for(&rendered, "current").split_whitespace().collect();
    let earlier_row = row_for(&rendered, "earlier");
    let earlier_fields: Vec<&str> = earlier_row.split_whitespace().collect();
    assert_eq!(current[2], "13:28", "{rendered}");
    assert_eq!(earlier_fields[2], "(03:07)", "{rendered}");
    assert!(
        rendered.contains("parentheses = an earlier session than the one running now"),
        "the earlier-session notation is unexplained:\n{rendered}"
    );

    // Provenance labels the old series; it never erases an observation from it.
    assert!(earlier_row.contains("[█]"), "{earlier_row:?}");
    assert!(
        !earlier_row.contains(GAP),
        "an earlier session's observed bucket became a gap: {earlier_row:?}"
    );

    let only_live = pane(
        &[recorded_by(
            activity("current", levels(&[5]), AgentState::Working, Some(4), 1),
            &live,
        )],
        &Config::default(),
        AS_OF,
        Some(&live),
    );
    assert!(
        !only_live.contains("parentheses ="),
        "a pane containing only the live session explains an unused notation:\n{only_live}"
    );
}

#[test]
fn an_unknown_session_is_bare_only_when_the_live_session_is_also_unknown() {
    let live = session("live-fingerprint", 13 * 3_600 + 28 * 60);
    let row = activity("unknown", levels(&[4]), AgentState::Idle, Some(1), 1);

    let with_live = pane(
        std::slice::from_ref(&row),
        &Config::default(),
        AS_OF,
        Some(&live),
    );
    let fields: Vec<&str> = row_for(&with_live, "unknown").split_whitespace().collect();
    assert_eq!(fields[2], "(?)", "{with_live}");
    assert!(
        with_live.contains("? = that session's start could not be established"),
        "the unknown marker is unexplained:\n{with_live}"
    );

    let without_live = pane(&[row], &Config::default(), AS_OF, None);
    let fields: Vec<&str> = row_for(&without_live, "unknown")
        .split_whitespace()
        .collect();
    assert_eq!(fields[2], "?", "{without_live}");
}

#[test]
fn no_live_session_means_no_row_is_called_an_earlier_one() {
    // `--once` with herdr not running, or with no `HERDR_SOCKET_PATH` exported:
    // the socket cannot be fingerprinted, so nothing is comparable to anything.
    // Marking every row "an earlier session than the one running now" would be a
    // claim about a session that was never established.
    let earlier = session("earlier-fingerprint", 3 * 3_600 + 7 * 60);
    let rendered = pane(
        &[recorded_by(
            activity("orphan", levels(&[6]), AgentState::Working, Some(9), 1),
            &earlier,
        )],
        &Config::default(),
        AS_OF,
        None,
    );

    let fields: Vec<&str> = row_for(&rendered, "orphan").split_whitespace().collect();
    assert_eq!(
        fields[2], "03:07",
        "the row states when its own session began and nothing more:\n{rendered}"
    );
    assert!(
        !rendered.contains("parentheses ="),
        "there is no live session to be earlier than:\n{rendered}"
    );
    assert!(
        rendered.contains("could not be established"),
        "and the pane says the live session is unknown:\n{rendered}"
    );
}

#[test]
fn a_parenthesised_session_does_not_shift_the_columns_after_it() {
    let live = session("live-fingerprint", 13 * 3_600 + 28 * 60);
    let earlier = session("earlier-fingerprint", 3 * 3_600 + 7 * 60);
    let rendered = pane(
        &[
            recorded_by(
                activity("live", levels(&[4]), AgentState::Working, Some(1), 1),
                &live,
            ),
            recorded_by(
                activity("old", levels(&[4]), AgentState::Working, Some(1), 1),
                &earlier,
            ),
        ],
        &Config::default(),
        AS_OF,
        Some(&live),
    );

    let state_offsets: Vec<usize> = ["live", "old"]
        .iter()
        .map(|label| {
            let row = row_for(&rendered, label);
            let state = row
                .find("> working")
                .unwrap_or_else(|| panic!("no state column in {row:?}"));
            display_width(&row[..state])
        })
        .collect();
    assert_eq!(state_offsets[0], state_offsets[1], "\n{rendered}");
}

#[test]
fn pane_columns_line_up_when_labels_differ_in_width() {
    let rendered = sample_pane(vec![
        activity("web", levels(&[0, 1, 2, 3]), AgentState::Idle, Some(30), 1),
        activity(
            "a-very-long-workspace-name",
            levels(&[8, 8, 8, 8]),
            AgentState::Working,
            Some(4_800),
            4,
        ),
        activity("api", vec![None; 4], AgentState::Blocked, None, 2),
    ]);

    let starts = column_offsets(&rendered, '[');
    assert_eq!(starts.len(), 3, "one bracketed sparkline per workspace");
    assert!(
        starts.windows(2).all(|pair| pair[0] == pair[1]),
        "sparklines start at different columns: {starts:?}\n{rendered}"
    );

    let ends = column_offsets(&rendered, ']');
    assert!(
        ends.windows(2).all(|pair| pair[0] == pair[1]),
        "state columns start at different offsets: {ends:?}\n{rendered}"
    );
}

#[test]
fn pane_columns_line_up_when_a_label_contains_multi_byte_characters() {
    // `str::len()` would call the CJK label 33 bytes and the ASCII one 3, and
    // pad the wrong one — the exact silent misalignment this guards.
    let rendered = sample_pane(vec![
        activity("web", levels(&[1, 2, 3, 4]), AgentState::Idle, Some(1), 1),
        activity(
            "日本語のワークスペース",
            levels(&[4, 3, 2, 1]),
            AgentState::Working,
            Some(2),
            1,
        ),
        activity(
            "café-e\u{0301}dition",
            levels(&[0, 0, 0, 0]),
            AgentState::Done,
            Some(3),
            1,
        ),
        activity(
            "🔥-hot",
            levels(&[8, 8, 8, 8]),
            AgentState::Blocked,
            None,
            9,
        ),
    ]);

    let starts = column_offsets(&rendered, '[');
    assert_eq!(starts.len(), 4);
    assert!(
        starts.windows(2).all(|pair| pair[0] == pair[1]),
        "multi-byte labels misaligned the sparkline column: {starts:?}\n{rendered}"
    );
}

#[test]
fn pane_columns_line_up_when_sparklines_contain_gaps() {
    // A gapped series is the same width as any other, so a row that begins or
    // ends unobserved must sit in exactly the same columns as one that does not.
    let rendered = sample_pane(vec![
        activity(
            "a",
            vec![None, None, Some(Level(8)), Some(Level(8))],
            AgentState::Working,
            Some(10),
            1,
        ),
        activity(
            "bbbbbbbb",
            vec![Some(Level(1)), None, None, None],
            AgentState::Idle,
            Some(10),
            1,
        ),
    ]);
    let starts = column_offsets(&rendered, '[');
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0], starts[1]);
    // The brackets are what make a leading or trailing gap visible at all.
    assert!(rendered.contains(&format!("[{GAP}{GAP}██]")));
    assert!(rendered.contains(&format!("[▁{GAP}{GAP}{GAP}]")));
}

#[test]
fn a_marked_workspace_emits_an_aligned_line_below_its_sparkline() {
    let rendered = sample_pane(vec![with_transitions(
        activity("web", levels(&[1, 2, 3, 4]), AgentState::Idle, Some(5), 1),
        vec![Some(0), Some(5), Some(0), Some(1)],
    )]);
    let row = row_for(&rendered, "web");
    let markers =
        line_after(&rendered, row).unwrap_or_else(|| panic!("no marker line after {row:?}"));

    assert_eq!(marked_columns(row, markers), [1, 3], "\n{rendered}");
    assert_eq!(
        format!("{row}\n{markers}"),
        concat!(
            "web        [▁▂▃▄]    ?        - idle  0s       5s   0s    1\n",
            "             ^ ^"
        )
    );
    assert!(
        rendered.contains("^ busiest observed changes, not every change"),
        "{rendered}"
    );
}

#[test]
fn a_row_without_observed_transitions_emits_no_marker_line_or_legend_clause() {
    let rendered = sample_pane(vec![activity(
        "still",
        levels(&[1, 2, 3]),
        AgentState::Working,
        Some(4),
        1,
    )]);

    assert!(!rendered.contains(TRANSITION_MARKER), "{rendered}");
    assert!(!rendered.contains("busiest observed changes"), "{rendered}");
}

#[test]
fn no_more_than_three_transition_columns_are_marked_and_newest_ties_win() {
    let rendered = sample_pane(vec![with_transitions(
        activity(
            "busy",
            levels(&[1, 1, 1, 1, 1, 1]),
            AgentState::Working,
            Some(4),
            1,
        ),
        vec![Some(2); 6],
    )]);
    let row = row_for(&rendered, "busy");
    let markers =
        line_after(&rendered, row).unwrap_or_else(|| panic!("no marker line after {row:?}"));

    assert_eq!(markers.matches(TRANSITION_MARKER).count(), 3, "{markers:?}");
    assert_eq!(marked_columns(row, markers), [3, 4, 5], "\n{rendered}");
}

#[test]
fn an_unobserved_column_is_never_marked_between_observed_neighbours() {
    let rendered = sample_pane(vec![with_transitions(
        activity(
            "gapped",
            vec![Some(Level(4)), None, Some(Level(4))],
            AgentState::Working,
            Some(4),
            1,
        ),
        vec![Some(3), None, Some(2)],
    )]);
    let row = row_for(&rendered, "gapped");
    let markers =
        line_after(&rendered, row).unwrap_or_else(|| panic!("no marker line after {row:?}"));

    assert_eq!(marked_columns(row, markers), [0, 2], "\n{rendered}");
    assert!(row.contains(&format!("[▄{GAP}▄]")), "{row:?}");
}

#[test]
fn transition_markers_align_with_multibyte_sparklines_and_a_long_label() {
    let rendered = sample_pane(vec![with_transitions(
        activity(
            "日本語のとても長いワークスペース名",
            levels(&[1, 2, 3, 4]),
            AgentState::Working,
            Some(4),
            1,
        ),
        vec![Some(0), Some(0), Some(7), Some(0)],
    )]);
    let row = rendered
        .lines()
        .find(|line| line.contains("[▁▂▃▄]"))
        .unwrap_or_else(|| panic!("no workspace row in\n{rendered}"));
    let markers =
        line_after(&rendered, row).unwrap_or_else(|| panic!("no marker line after {row:?}"));

    assert_eq!(marked_columns(row, markers), [2], "\n{rendered}");
    assert_eq!(
        display_width(&markers[..markers.find(TRANSITION_MARKER).unwrap()]),
        display_width(&row[..row.rfind('[').unwrap()]) + 3
    );
}

#[test]
fn agent_rows_follow_their_workspace_in_most_recent_order() {
    let live = session("live", 13 * 3_600 + 28 * 60);
    let workspace = recorded_by(
        with_agents(
            activity(
                "web",
                levels(&[8, 8, 8, 8]),
                AgentState::Blocked,
                Some(60),
                2,
            ),
            vec![
                agent(
                    "pane-older",
                    None,
                    levels(&[0, 0, 0, 0]),
                    AgentState::Idle,
                    40,
                    0,
                    60,
                ),
                agent(
                    "pane-newer",
                    Some("claude"),
                    levels(&[8, 8, 8, 8]),
                    AgentState::Blocked,
                    3,
                    12,
                    60,
                ),
            ],
        ),
        &live,
    );
    let rendered = pane(&[workspace], &Config::default(), AS_OF, Some(&live));

    let workspace_at = rendered.find("\nweb").unwrap();
    let newer_at = rendered.find("\n  claude").unwrap();
    let older_at = rendered.find("\n  pane-older").unwrap();
    assert!(workspace_at < newer_at && newer_at < older_at, "{rendered}");

    let newer = agent_row_for(&rendered, "claude");
    assert!(newer.contains("! blocked"), "{newer:?}");
    assert!(newer.contains("12s"), "{newer:?}");
    assert!(newer.contains("3s"), "{newer:?}");
    assert!(
        !newer.contains("13:28"),
        "the agent repeated its workspace session: {newer:?}"
    );
    assert!(
        !newer.ends_with(' '),
        "the omitted aggregate count left trailing padding: {newer:?}"
    );
}

#[test]
fn an_agent_that_appeared_late_keeps_gaps_before_its_first_bar() {
    let workspace = with_agents(
        activity(
            "web",
            levels(&[8, 8, 8, 8]),
            AgentState::Working,
            Some(1),
            1,
        ),
        vec![agent(
            "pane-1",
            Some("claude"),
            vec![None, None, Some(Level::new(0)), Some(Level::new(8))],
            AgentState::Working,
            1,
            0,
            120,
        )],
    );
    let rendered = sample_pane(vec![workspace]);
    let row = agent_row_for(&rendered, "claude");

    assert!(
        row.contains(&format!("[{GAP}{GAP}{QUIET}█]")),
        "absence before appearance became observed quiet: {row:?}"
    );
}

#[test]
fn an_agent_row_gets_its_own_transition_markers() {
    let workspace = with_agents(
        activity(
            "web",
            levels(&[8, 8, 8, 8]),
            AgentState::Working,
            Some(1),
            1,
        ),
        vec![with_agent_transitions(
            agent(
                "pane-1",
                Some("claude"),
                levels(&[1, 2, 3, 4]),
                AgentState::Working,
                1,
                0,
                120,
            ),
            vec![Some(0), Some(4), Some(0), Some(2)],
        )],
    );
    let rendered = sample_pane(vec![workspace]);
    let workspace_row = row_for(&rendered, "web");
    let agent_row = agent_row_for(&rendered, "claude");
    let markers = line_after(&rendered, agent_row)
        .unwrap_or_else(|| panic!("no marker line after {agent_row:?}"));

    assert!(
        !line_after(&rendered, workspace_row).is_some_and(|line| line.contains(TRANSITION_MARKER)),
        "the aggregate borrowed its agent's marks:\n{rendered}"
    );
    assert_eq!(marked_columns(agent_row, markers), [1, 3], "\n{rendered}");
}

#[test]
fn short_and_long_agent_programs_keep_every_table_column_aligned() {
    let workspace = with_agents(
        activity(
            "web",
            levels(&[8, 8, 8, 8]),
            AgentState::Working,
            Some(1),
            2,
        ),
        vec![
            agent(
                "pane-short",
                Some("go"),
                levels(&[1, 2, 3, 4]),
                AgentState::Working,
                1,
                0,
                60,
            ),
            agent(
                "pane-long",
                Some("a-program-name-that-is-deliberately-much-too-long"),
                levels(&[4, 3, 2, 1]),
                AgentState::Idle,
                2,
                0,
                60,
            ),
        ],
    );
    let rendered = sample_pane(vec![workspace]);
    let rows = [
        row_for(&rendered, "web"),
        agent_row_for(&rendered, "go"),
        agent_row_for(&rendered, "a-program"),
    ];

    let activity_offsets: Vec<usize> = rows
        .iter()
        .map(|row| display_width(&row[..row.rfind('[').unwrap()]))
        .collect();
    assert!(
        activity_offsets.windows(2).all(|pair| pair[0] == pair[1]),
        "activity columns are not aligned: {activity_offsets:?}\n{rendered}"
    );

    let state_offsets: Vec<usize> = rows
        .iter()
        .map(|row| {
            let at = row
                .find("> working")
                .or_else(|| row.find("- idle"))
                .unwrap();
            display_width(&row[..at])
        })
        .collect();
    assert!(
        state_offsets.windows(2).all(|pair| pair[0] == pair[1]),
        "state columns are not aligned: {state_offsets:?}\n{rendered}"
    );
}

#[test]
fn the_single_agent_legend_clause_only_appears_with_agent_rows() {
    const CLAUSE: &str = "indented rows = single agents";

    let without_agents = sample_pane(vec![activity(
        "web",
        levels(&[1]),
        AgentState::Working,
        Some(1),
        1,
    )]);
    assert!(!without_agents.contains(CLAUSE), "{without_agents}");

    let with_agent = sample_pane(vec![with_agents(
        activity("web", levels(&[1]), AgentState::Working, Some(1), 1),
        vec![agent(
            "pane-1",
            Some("claude"),
            levels(&[1]),
            AgentState::Working,
            1,
            0,
            60,
        )],
    )]);
    assert!(with_agent.contains(CLAUSE), "{with_agent}");
    assert!(
        with_agent.contains("not there yet, not idle"),
        "the legend does not protect the pre-appearance gap meaning:\n{with_agent}"
    );
}

#[test]
fn a_workspace_without_agent_rings_keeps_the_original_pane() {
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[1]),
        AgentState::Idle,
        Some(5),
        1,
    )]);
    let expected = "\
pulse — 1 workspace — 03:06:40 UTC

workspace  activity  session  state   blocked  for  seen  agents
web        [▁]       ?        - idle  0s       5s   0s    1

legend  ▁▂▃▄▅▆▇█ busier  |  · observed, nothing happened  |  ╌ not observed
        for = how long the state had held when last seen  |  seen = how long ago that was
        blocked = estimated from the samples that saw a blocked agent  |  measured over time actually watched, not the whole row
        session = when the herdr session that recorded the row began  |  the session running now could not be established, so no row is marked live  |  ? = that session's start could not be established
";

    assert_eq!(rendered, expected);
}

#[test]
fn a_pane_with_nothing_recorded_says_so_rather_than_drawing_an_empty_table() {
    let rendered = pane(&[], &Config::default(), AS_OF, None);
    assert!(rendered.to_lowercase().contains("no workspace history"));
    assert!(rendered.contains("--enable"));
    // An empty table would read as "we looked, and every workspace was quiet".
    assert!(!rendered.contains("workspace  "));
}

#[test]
fn the_pane_explains_what_a_gap_column_means() {
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[1, 2]),
        AgentState::Idle,
        Some(5),
        1,
    )]);
    let lower = rendered.to_lowercase();
    assert!(lower.contains("legend"));
    assert!(lower.contains("nothing happened"));

    // Asserted against the constants rather than against prose. An earlier
    // version of this test pinned the literal words "blank not observed", which
    // silently became a lie the moment GAP stopped being a space — the legend
    // is precisely the thing that must never drift from the glyphs it explains.
    assert!(
        rendered.contains(GAP),
        "the key never shows the gap glyph itself:\n{rendered}"
    );
    assert!(
        rendered.contains(QUIET),
        "the key never shows the quiet glyph itself:\n{rendered}"
    );
    assert!(
        rendered.contains("not observed"),
        "the key does not say what a gap means:\n{rendered}"
    );
    for step in RAMP {
        assert!(
            rendered.contains(step),
            "the key omits ramp step {step:?}:\n{rendered}"
        );
    }
}

#[test]
fn the_pane_carries_the_state_word_its_glyph_and_a_duration() {
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[1, 2]),
        AgentState::Blocked,
        Some(4_800),
        3,
    )]);
    assert!(rendered.contains("! blocked"));
    assert!(rendered.contains("1h20"));
    assert!(rendered.contains("web"));
}

#[test]
fn an_unobserved_state_duration_shows_a_question_mark_not_a_zero() {
    // `state_for` is `None` when no transition has been seen. Rendering that as
    // `0s` would claim the state changed this instant, which is a different fact
    // from not knowing when it changed.
    let rendered = sample_pane(vec![seen_ago(
        activity("web", levels(&[1]), AgentState::Unknown, None, 0),
        5,
    )]);
    let row = row_for(&rendered, "web");
    // By field, not by substring: "5s" contains "0s" only by accident of digits,
    // and an assertion that can be satisfied by a neighbouring column is not an
    // assertion about this one.
    let fields: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        &fields[fields.len() - 3..],
        // for = unknown, seen = five seconds ago, agents = none.
        &["?", "5s", "0"],
        "{row:?}"
    );
}

#[test]
fn blocked_time_renders_as_a_duration_in_its_own_column() {
    let rendered = sample_pane(vec![with_blocked_time(
        activity("web", levels(&[1, 2]), AgentState::Blocked, Some(30), 1),
        4_800,
        7_200,
    )]);
    let cells: Vec<&str> = row_for(&rendered, "web")
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();

    assert_eq!(cells[4], "1h20", "{rendered}");
}

#[test]
fn blocked_time_is_unknown_when_nothing_was_watched() {
    // The session column also renders `?` when there is no live session, so the
    // marker alone proves nothing about which column it came from. State and
    // duration are given known values and the neighbours are asserted, so this
    // fails if the column moves or disappears.
    let rendered = sample_pane(vec![with_blocked_time(
        activity("web", vec![None; 2], AgentState::Working, Some(30), 1),
        0,
        0,
    )]);
    let cells: Vec<&str> = row_for(&rendered, "web")
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();

    assert_eq!(cells[3], "> working", "{rendered}");
    assert_eq!(cells[4], "?", "{rendered}");
    assert_eq!(cells[5], "30s", "{rendered}");
}

#[test]
fn the_week_pane_reports_the_weeks_blocked_time_and_not_the_afternoons() {
    // The figure travels with the series it describes. Left behind, a week row
    // would print the fine ring's last few hours beside a seven-day sparkline,
    // under a legend saying the figure covers the time watched in this row.
    let mut workspace = activity("web", vec![None; 2], AgentState::Working, Some(30), 1);
    workspace.blocked_seconds = 60;
    workspace.watched_seconds = 120;
    workspace.week = levels(&[4; WEEK_COLUMNS]);
    workspace.week_blocked_seconds = 7_200;
    workspace.week_watched_seconds = 86_400;

    let week = week_pane(&[workspace.clone()], &Config::default(), AS_OF, None);
    let once = pane(&[workspace], &Config::default(), AS_OF, None);

    let cell = |rendered: &str| {
        row_for(rendered, "web")
            .split("  ")
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .nth(4)
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(cell(&week), "2h00", "{week}");
    assert_eq!(cell(&once), "1m", "{once}");
}

#[test]
fn zero_blocked_time_is_an_observation_when_time_was_watched() {
    let rendered = sample_pane(vec![with_blocked_time(
        activity("web", levels(&[0, 0]), AgentState::Idle, Some(30), 1),
        0,
        120,
    )]);
    let cells: Vec<&str> = row_for(&rendered, "web")
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();

    assert_eq!(cells[4], "0s", "{rendered}");
}

#[test]
fn the_blocked_legend_appears_only_when_a_row_was_watched() {
    const CLAUSE: &str = "blocked = estimated from the samples that saw a blocked agent  |  \
                          measured over time actually watched, not the whole row";

    let watched = sample_pane(vec![with_blocked_time(
        activity("web", levels(&[0]), AgentState::Idle, Some(1), 1),
        0,
        60,
    )]);
    assert!(
        watched.contains(CLAUSE),
        "a blocked figure is unexplained:\n{watched}"
    );

    let unwatched = sample_pane(vec![with_blocked_time(
        activity("web", vec![None], AgentState::Unknown, None, 0),
        0,
        0,
    )]);
    assert!(
        !unwatched.contains(CLAUSE),
        "a pane with no blocked figure explains one anyway:\n{unwatched}"
    );
}

#[test]
fn the_blocked_column_and_the_columns_after_it_stay_aligned() {
    let rendered = sample_pane(vec![
        with_blocked_time(
            activity("short", levels(&[4]), AgentState::Blocked, Some(240), 1),
            70,
            120,
        ),
        with_blocked_time(
            seen_ago(
                activity(
                    "a-longer-label",
                    levels(&[8]),
                    AgentState::Working,
                    Some(540),
                    2,
                ),
                18_000,
            ),
            3,
            120,
        ),
    ]);
    let header = rendered
        .lines()
        .find(|line| line.starts_with("workspace"))
        .unwrap_or_default();
    let short = row_for(&rendered, "short");
    let longer = row_for(&rendered, "a-longer-label");

    let blocked_offsets = [
        display_width(&header[..header.find("blocked").unwrap()]),
        display_width(&short[..short.find("1m").unwrap()]),
        display_width(&longer[..longer.find("3s").unwrap()]),
    ];
    assert!(
        blocked_offsets.windows(2).all(|pair| pair[0] == pair[1]),
        "blocked cells do not line up: {blocked_offsets:?}\n{rendered}"
    );

    let for_offsets = [
        display_width(&header[..header.find("for").unwrap()]),
        display_width(&short[..short.find("4m").unwrap()]),
        display_width(&longer[..longer.find("9m").unwrap()]),
    ];
    assert!(
        for_offsets.windows(2).all(|pair| pair[0] == pair[1]),
        "cells after blocked do not line up: {for_offsets:?}\n{rendered}"
    );
}

// ---------------------------------------------------------------------------
// freshness: what the pane may say in the present tense
// ---------------------------------------------------------------------------

#[test]
fn a_state_nobody_has_observed_for_hours_is_not_reported_in_the_present_tense() {
    // The exact row from the review: five hours of gaps beside the claim that an
    // agent has been working for five hours. The sparkline was the honest half,
    // and the words now agree with it.
    let stale = seen_ago(
        activity("alpha", vec![None; 32], AgentState::Working, Some(600), 1),
        5 * 3_600,
    );
    let rendered = sample_pane(vec![stale]);
    let row = row_for(&rendered, "alpha");

    assert!(
        row.contains("was working"),
        "a five-hour-old observation is stated as fact: {row:?}"
    );
    // The age of the observation, not a duration extrapolated across it.
    assert!(row.contains("5h00"), "{row:?}");
}

#[test]
fn a_freshly_observed_state_is_reported_in_the_present_tense() {
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[4, 5]),
        AgentState::Working,
        Some(600),
        1,
    )]);
    let row = row_for(&rendered, "web");
    assert!(row.contains("> working"), "{row:?}");
    assert!(
        !row.contains("was"),
        "a current observation was hedged: {row:?}"
    );
}

#[test]
fn the_freshness_line_falls_exactly_on_the_tolerance() {
    let config = Config::default();
    let tolerance = staleness_tolerance(&config);

    let at = pane(
        &[seen_ago(
            activity("edge", levels(&[4]), AgentState::Working, Some(60), 1),
            tolerance,
        )],
        &config,
        AS_OF,
        None,
    );
    assert!(
        !row_for(&at, "edge").contains("was"),
        "a workspace exactly at the tolerance is still current:\n{at}"
    );

    let past = pane(
        &[seen_ago(
            activity("edge", levels(&[4]), AgentState::Working, Some(60), 1),
            tolerance + 1,
        )],
        &config,
        AS_OF,
        None,
    );
    assert!(
        row_for(&past, "edge").contains("was working"),
        "one second past the tolerance is stale:\n{past}"
    );
}

#[test]
fn the_tolerance_is_three_sampling_intervals_not_a_fixed_number_of_seconds() {
    // Lateness is only meaningful in cycles missed. A user sampling once a minute
    // must not be told their whole session is stale between two healthy cycles.
    for seconds in [1u64, 5, 30, 60, 3_600] {
        let config = Config {
            interval: Duration::from_secs(seconds),
            ..Config::default()
        };
        assert_eq!(staleness_tolerance(&config), seconds * 3);
    }
    // Never zero, or every row would be stale the instant it was recorded.
    assert!(staleness_tolerance(&Config::default()) >= 1);
}

#[test]
fn a_workspace_that_has_never_been_observed_is_never_current() {
    let rendered = sample_pane(vec![never_seen(activity(
        "ghost",
        vec![None; 4],
        AgentState::Unknown,
        None,
        0,
    ))]);
    let row = row_for(&rendered, "ghost");
    assert!(row.contains("was unknown"), "{row:?}");
    // Both "how long in that state" and "how long ago" are genuinely unknown, and
    // both say so rather than defaulting to a number. Checked by field rather
    // than by counting `?`, since the glyph for `Unknown` is one too.
    let fields: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(&fields[fields.len() - 3..], &["?", "?", "0"], "{row:?}");
}

#[test]
fn a_clock_that_runs_behind_the_history_reports_a_fresh_observation_not_a_huge_age() {
    // `observed_ago` saturates. A history written by a machine whose clock is
    // ahead of ours must not read as an observation from the far future turned
    // into an enormous negative age.
    let ahead = WorkspaceActivity {
        last_seen: Some(AS_OF + 10_000),
        ..activity("ahead", levels(&[4]), AgentState::Working, Some(60), 1)
    };
    let rendered = sample_pane(vec![ahead]);
    let row = row_for(&rendered, "ahead");
    assert!(row.contains("> working"), "{row:?}");
    assert!(row.contains("0s"), "{row:?}");
}

#[test]
fn the_legend_explains_the_past_tense_only_when_a_row_uses_it() {
    let fresh = sample_pane(vec![activity(
        "web",
        levels(&[4]),
        AgentState::Working,
        Some(60),
        1,
    )]);
    assert!(
        !fresh.contains("not a claim about now"),
        "a pane with no stale rows explains a distinction none of them makes:\n{fresh}"
    );

    let stale = sample_pane(vec![seen_ago(
        activity("web", levels(&[4]), AgentState::Working, Some(60), 1),
        9_000,
    )]);
    assert!(
        stale.contains("not a claim about now"),
        "a stale row is unexplained:\n{stale}"
    );
    // And the two duration columns are always distinguished.
    assert!(stale.contains("seen = how long ago"), "{stale}");
}

#[test]
fn a_stale_row_and_a_fresh_row_stay_aligned() {
    // The past-tense marker widens the state cell, so every other row has to move
    // with it or the pane loses the one property it is judged on.
    let rendered = sample_pane(vec![
        activity(
            "fresh",
            levels(&[4, 5, 6, 7]),
            AgentState::Idle,
            Some(60),
            1,
        ),
        seen_ago(
            activity("stale", vec![None; 4], AgentState::Working, Some(60), 1),
            18_000,
        ),
    ]);
    let ends = column_offsets(&rendered, ']');
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0], ends[1], "\n{rendered}");

    // Header and rows share the column, so the agent counts line up too.
    let widths: Vec<usize> = rendered
        .lines()
        .filter(|line| line.contains('['))
        .map(display_width)
        .collect();
    assert_eq!(widths[0], widths[1], "\n{rendered}");
}

#[test]
fn the_pane_labels_its_timescale_only_when_the_series_matches_the_configuration() {
    let config = Config::default();
    let (columns, buckets_per_column) = pane_geometry(&config);
    assert_eq!(columns, 32);
    assert_eq!(buckets_per_column, 7);

    let matching = pane(
        &[activity(
            "web",
            levels(&vec![2u8; columns]),
            AgentState::Working,
            Some(60),
            1,
        )],
        &config,
        0,
        None,
    );
    assert!(matching.contains("one column = 7m"), "{matching}");
    assert!(matching.contains("whole row = 3h44"), "{matching}");

    // A series of some other shape would make that axis a lie, so it is omitted.
    let mismatched = sample_pane(vec![activity(
        "web",
        levels(&[2, 2, 2]),
        AgentState::Working,
        Some(60),
        1,
    )]);
    assert!(!mismatched.contains("one column ="), "{mismatched}");
}

#[test]
fn the_week_pane_draws_the_week_series_instead_of_the_fine_series() {
    let config = Config::default();
    let (columns, _) = pane_geometry(&config);
    let mut workspace = activity("web", vec![None; columns], AgentState::Working, Some(60), 1);
    workspace.week = levels(&[8; WEEK_COLUMNS]);

    let rendered = week_pane(&[workspace], &config, AS_OF, None);
    let row = row_for(&rendered, "web");
    assert!(
        row.contains(&format!("[{}]", "█".repeat(WEEK_COLUMNS))),
        "the busy week was not drawn: {row:?}"
    );
    assert!(
        !row.contains(&GAP.to_string().repeat(columns)),
        "the all-gap fine series was drawn instead: {row:?}"
    );
    assert!(
        rendered.starts_with("pulse — week view — 1 workspace — "),
        "the week pane is not identifiable from its header: {rendered}"
    );
}

#[test]
fn the_week_pane_marks_week_transitions_not_the_fine_windows_transitions() {
    let mut workspace = with_transitions(
        activity(
            "web",
            levels(&[8, 8, 8, 8]),
            AgentState::Working,
            Some(60),
            1,
        ),
        vec![Some(9), Some(0), Some(0), Some(0)],
    );
    workspace.week = levels(&[8; WEEK_COLUMNS]);
    let mut week_transitions = vec![Some(0); WEEK_COLUMNS];
    week_transitions[5] = Some(2);
    let workspace = with_week_transitions(workspace, week_transitions);

    let rendered = week_pane(&[workspace], &Config::default(), AS_OF, None);
    let row = row_for(&rendered, "web");
    let markers =
        line_after(&rendered, row).unwrap_or_else(|| panic!("no marker line after {row:?}"));

    assert_eq!(marked_columns(row, markers), [5], "\n{rendered}");
}

#[test]
fn the_week_pane_keeps_a_gap_distinct_from_an_observed_quiet_hour() {
    let mut workspace = activity("web", levels(&[8]), AgentState::Idle, Some(60), 1);
    workspace.week = vec![None; WEEK_COLUMNS];
    workspace.week[1] = Some(Level::new(0));

    let rendered = week_pane(&[workspace], &Config::default(), AS_OF, None);
    let row = row_for(&rendered, "web");
    assert!(
        row.contains(&format!("[{GAP}{QUIET}")),
        "gap and observed quiet were not drawn distinctly: {row:?}"
    );
}

#[test]
fn the_week_and_once_legends_state_the_scale_of_the_series_they_draw() {
    let config = Config::default();
    let (columns, _) = pane_geometry(&config);
    let mut workspace = activity(
        "web",
        levels(&vec![2; columns]),
        AgentState::Working,
        Some(60),
        1,
    );
    workspace.week = levels(&[2; WEEK_COLUMNS]);

    let once = pane(std::slice::from_ref(&workspace), &config, AS_OF, None);
    let week = week_pane(&[workspace], &config, AS_OF, None);

    assert!(
        once.contains("one column = 7m  |  whole row = 3h44"),
        "{once}"
    );
    assert!(
        !once.contains("one column = 6h00  |  whole row = 7d"),
        "{once}"
    );
    assert!(
        week.contains("one column = 6h00  |  whole row = 7d"),
        "{week}"
    );
    assert!(
        !week.contains("one column = 7m  |  whole row = 3h44"),
        "{week}"
    );
}

#[test]
fn pane_geometry_never_asks_for_more_columns_than_there_are_buckets() {
    for retention in [1usize, 8, 31, 32, 33, 240, 10_000] {
        let config = Config {
            retention_buckets: retention,
            ..Config::default()
        };
        let (columns, per_column) = pane_geometry(&config);
        assert!(columns >= 1 && per_column >= 1, "retention {retention}");
        assert!(columns <= retention.max(1), "retention {retention}");
        assert!(columns <= 32);
    }
}

#[test]
fn a_control_character_in_a_label_cannot_break_the_layout() {
    // Labels come from the user. A newline or a carriage return would move the
    // cursor and undo every column we just aligned.
    let rendered = sample_pane(vec![
        activity(
            "evil\nlabel\r\u{1b}[31m",
            levels(&[1, 2, 3, 4]),
            AgentState::Idle,
            Some(1),
            1,
        ),
        activity("plain", levels(&[1, 2, 3, 4]), AgentState::Idle, Some(1), 1),
    ]);
    let rows: Vec<&str> = rendered.lines().filter(|line| line.contains('[')).collect();
    assert_eq!(rows.len(), 2, "one row per workspace:\n{rendered}");
    let starts = column_offsets(&rendered, '[');
    assert_eq!(starts[0], starts[1]);
    assert!(!rendered.contains('\r'));
}

#[test]
fn an_empty_label_is_named_rather_than_left_blank() {
    let rendered = sample_pane(vec![activity(
        "   ",
        levels(&[1]),
        AgentState::Idle,
        Some(1),
        1,
    )]);
    assert!(rendered.contains("(unnamed)"), "{rendered}");
}

#[test]
fn an_over_long_label_is_elided_instead_of_widening_every_row() {
    let long = "x".repeat(200);
    let rendered = sample_pane(vec![
        activity(&long, levels(&[1, 2]), AgentState::Idle, Some(1), 1),
        activity("web", levels(&[1, 2]), AgentState::Idle, Some(1), 1),
    ]);
    assert!(rendered.contains('…'));
    let starts = column_offsets(&rendered, '[');
    assert_eq!(starts[0], starts[1]);
    // 28 columns of label plus the two-space column separator.
    assert!(
        starts[0] <= 30,
        "a single label widened the whole pane to {}",
        starts[0]
    );
}

#[test]
fn the_pane_never_panics_on_awkward_input() {
    let config = Config {
        retention_buckets: 8,
        bucket_seconds: 10,
        interval: Duration::from_secs(1),
        ..Config::default()
    };

    let rows = vec![
        never_seen(activity("", vec![], AgentState::Unknown, None, 0)),
        activity(
            "gaps",
            vec![None; 500],
            AgentState::Done,
            Some(u64::MAX),
            999,
        ),
        activity(
            "huge",
            levels(&[u8::MAX; 40]),
            AgentState::Blocked,
            Some(0),
            1,
        ),
        // A last_seen far ahead of every `as_of` below, so the saturating age
        // path is exercised alongside the enormous-age one.
        WorkspaceActivity {
            last_seen: Some(u64::MAX),
            ..activity("ahead", levels(&[3]), AgentState::Working, Some(1), 1)
        },
    ];
    for as_of in [0u64, 1, 86_399, u64::MAX] {
        let rendered = pane(&rows, &config, as_of, None);
        assert!(!rendered.is_empty());
    }
}

#[test]
fn the_pane_clock_is_a_valid_time_of_day_for_any_timestamp() {
    for as_of in [0u64, 59, 3_600, 86_399, 86_400, u64::MAX] {
        let rendered = pane(
            &[activity("web", levels(&[1]), AgentState::Idle, Some(1), 1)],
            &Config::default(),
            as_of,
            None,
        );
        let header = rendered.lines().next().unwrap_or_default();
        let clock = header
            .rsplit("— ")
            .next()
            .unwrap_or_default()
            .trim_end_matches(" UTC");
        let parts: Vec<u64> = clock.split(':').filter_map(|p| p.parse().ok()).collect();
        assert_eq!(parts.len(), 3, "unparseable clock in {header:?}");
        assert!(
            parts[0] < 24 && parts[1] < 60 && parts[2] < 60,
            "{header:?}"
        );
    }
}

/// herdr trims leading and trailing whitespace from a badge token's value, and
/// deletes a token whose value is entirely whitespace. Both were verified
/// against a live 0.8.0 server by reading the tokens back out of a subsequent
/// snapshot, and neither behaviour is documented.
///
/// That makes "is the gap glyph a printing character?" a correctness property
/// rather than a matter of taste. When [`GAP`] was a space:
///
///   * a badge ending in gaps lost its newest columns, so the sparkline silently
///     stopped being aligned to now and stale activity read as current;
///   * a badge that was entirely gaps rendered as the empty string, herdr
///     dropped the token, and the badge vanished at exactly the moment the
///     record became least trustworthy.
///
/// The entire suite passed with `GAP = ' '`, because every other assertion
/// refers to `GAP` symbolically and so moved along with the bug. This one pins
/// the property to the outside world instead.
#[test]
fn the_gap_glyph_survives_a_badge_round_trip() {
    assert!(
        !GAP.is_whitespace(),
        "GAP must be a printing character: herdr trims whitespace off token \
         values and deletes an all-whitespace token"
    );

    let config = Config::default();

    // A workspace with no observations at all deliberately gets no badge — the
    // daemon reads an empty string as "clear the token", so a workspace pulse
    // knows nothing about does not occupy a sidebar row. That is a different
    // thing from a badge that *renders* and then gets mangled in transit, which
    // is what the rest of this test is about.
    let all_gaps = activity(
        "quiet",
        vec![None; config.badge_columns],
        AgentState::Idle,
        None,
        1,
    );
    assert!(
        badge(&all_gaps, &config).is_empty(),
        "a never-observed workspace should clear its token, not draw a row of gaps"
    );

    // Every badge that *is* drawn must survive the round trip unchanged. A
    // badge herdr trims is a badge whose columns no longer line up with the
    // clock, and nothing on screen says so.
    for gaps in 0..config.badge_columns {
        let mut series = vec![Some(Level::new(4)); config.badge_columns];
        for slot in series.iter_mut().take(gaps) {
            *slot = None;
        }
        let leading = activity("lead", series, AgentState::Working, None, 1);
        let rendered = badge(&leading, &config);
        assert_eq!(
            rendered.trim(),
            rendered,
            "leading gaps were trimmed away with {gaps} of them: {rendered:?}"
        );
    }

    // A series whose *newest* columns are gaps must keep them, because those are
    // the columns that carry "we have stopped watching".
    let mut series = vec![Some(Level::new(6)); config.badge_columns];
    let last = series.len() - 1;
    series[last] = None;
    series[last - 1] = None;
    let trailing = activity("stalled", series, AgentState::Idle, None, 1);
    let rendered = badge(&trailing, &config);
    assert_eq!(
        rendered.trim(),
        rendered,
        "trailing gaps were trimmed away: {rendered:?}"
    );
    assert!(
        rendered.contains(GAP),
        "trailing gaps vanished from {rendered:?}"
    );
}

/// The `--json` document must carry the same gap/quiet distinction each pane
/// draws with glyphs, but in the JSON type rather than in a sentinel number: a
/// gap is `null`, an observed-but-idle bucket is `0`. A consumer that flattens
/// the two — treating `null` as `0` — gets exactly the wrong answer this plugin
/// exists to prevent, so the distinction has to survive serialisation in both
/// the fine and week histories.
#[test]
fn json_carries_both_series_with_the_gap_and_quiet_distinction_intact() {
    let config = Config::default();
    let series = vec![None, Some(Level::new(0)), Some(Level::new(5))];
    let mut workspace = activity("w1", series, AgentState::Idle, None, 1);
    workspace.week = vec![Some(Level::new(0)), None, Some(Level::new(8))];
    workspace.week_transitions = vec![Some(0), None, Some(0)];

    let document = json_document(&config, AS_OF, 3, 1, &[workspace], None, running());
    let series = &document["workspaces"][0]["series"];
    let week = &document["workspaces"][0]["week"];

    assert!(
        series[0].is_null(),
        "a fine gap must serialise as JSON null, not 0: {series}"
    );
    assert_eq!(
        series[1], 0,
        "a fine observed quiet bucket must serialise as 0, not null: {series}"
    );
    assert_eq!(
        series[2], 5,
        "a fine observed active bucket lost its level: {series}"
    );
    assert_eq!(
        week[0], 0,
        "a week observed quiet bucket must serialise as 0, not null: {week}"
    );
    assert!(
        week[1].is_null(),
        "a week gap must serialise as JSON null, not 0: {week}"
    );
    assert_eq!(
        week[2], 8,
        "a week observed active bucket lost its level: {week}"
    );
    assert_eq!(document["week_bucket_seconds"], WEEK_BUCKET_SECONDS);
    assert_eq!(document["week_columns"], WEEK_COLUMNS);
}

#[test]
fn json_carries_workspace_week_and_agent_transition_arrays_with_nulls_intact() {
    let mut workspace = with_transitions(
        activity(
            "w1",
            vec![None, Some(Level::new(0)), Some(Level::new(5))],
            AgentState::Working,
            Some(30),
            1,
        ),
        vec![None, Some(0), Some(7)],
    );
    workspace.week = vec![Some(Level::new(4)), None, Some(Level::new(8))];
    workspace.week_transitions = vec![Some(1), None, Some(0)];
    let workspace = with_agents(
        workspace,
        vec![with_agent_transitions(
            agent(
                "pane-7",
                Some("claude"),
                vec![None, Some(Level::new(4)), Some(Level::new(4))],
                AgentState::Working,
                7,
                0,
                120,
            ),
            vec![None, Some(3), Some(0)],
        )],
    );

    let document = json_document(
        &Config::default(),
        AS_OF,
        3,
        1,
        &[workspace],
        None,
        running(),
    );
    let workspace = &document["workspaces"][0];

    assert_eq!(workspace["transitions"], serde_json::json!([null, 0, 7]));
    assert_eq!(
        workspace["week_transitions"],
        serde_json::json!([1, null, 0])
    );
    assert_eq!(
        workspace["agents"][0]["transitions"],
        serde_json::json!([null, 3, 0])
    );
}

#[test]
fn json_carries_each_agent_series_without_flattening_its_gaps() {
    let workspace = with_agents(
        activity("w1", levels(&[8, 8, 8]), AgentState::Working, Some(30), 1),
        vec![agent(
            "pane-7",
            Some("claude"),
            vec![None, Some(Level::new(0)), Some(Level::new(8))],
            AgentState::Working,
            7,
            7,
            120,
        )],
    );

    let document = json_document(
        &Config::default(),
        AS_OF,
        3,
        1,
        &[workspace],
        None,
        running(),
    );

    assert_eq!(
        document["workspaces"][0]["agents"],
        serde_json::json!([{
            "pane_id": "pane-7",
            "program": "claude",
            "state": "working",
            "series": [null, 0, 8],
            "transitions": [null, 0, 0],
            "last_seen": AS_OF - 7,
            // The same two fields the workspace object carries: a consumer
            // reading `"state":"working"` otherwise has no way to see how old
            // the observation behind it is.
            "observed_ago_seconds": 7,
            "state_is_current": true,
            "blocked_seconds": 7,
            "watched_seconds": 120,
        }])
    );
}

#[test]
fn json_carries_blocked_time_and_the_watched_time_that_supports_it() {
    let config = Config::default();
    let workspace = with_blocked_time(
        activity("w1", levels(&[5]), AgentState::Blocked, Some(30), 1),
        42,
        120,
    );

    let document = json_document(&config, AS_OF, 1, 1, &[workspace], None, running());
    let workspace = &document["workspaces"][0];

    assert_eq!(workspace["blocked_seconds"], 42);
    assert_eq!(workspace["watched_seconds"], 120);
}

#[test]
fn json_carries_the_live_and_per_workspace_session_marks() {
    let config = Config::default();
    let live = session("live-fingerprint", 13 * 3_600 + 28 * 60);
    let earlier = session("earlier-fingerprint", 3 * 3_600 + 7 * 60);
    let mut current = recorded_by(
        activity(
            "checkout-live",
            levels(&[4]),
            AgentState::Working,
            Some(1),
            1,
        ),
        &live,
    );
    let mut old = recorded_by(
        activity("checkout-old", levels(&[8]), AgentState::Done, Some(1), 0),
        &earlier,
    );
    // The same checkout can have one independent series from each herdr run.
    current.workspace_id = "same-checkout".to_string();
    old.workspace_id = "same-checkout".to_string();

    let document = json_document(
        &config,
        AS_OF,
        1,
        1,
        &[current, old],
        Some(&live),
        running(),
    );
    assert_eq!(
        document["session"]["fingerprint"].as_str(),
        Some("live-fingerprint")
    );
    assert_eq!(
        document["session"]["began"].as_u64(),
        Some(13 * 3_600 + 28 * 60)
    );

    let current_session = &document["workspaces"][0]["session"];
    assert_eq!(
        current_session["fingerprint"].as_str(),
        Some("live-fingerprint")
    );
    assert_eq!(
        current_session["began"].as_u64(),
        Some(13 * 3_600 + 28 * 60)
    );
    assert_eq!(current_session["is_current"].as_bool(), Some(true));

    let earlier_session = &document["workspaces"][1]["session"];
    assert_eq!(
        earlier_session["fingerprint"].as_str(),
        Some("earlier-fingerprint")
    );
    assert_eq!(earlier_session["began"].as_u64(), Some(3 * 3_600 + 7 * 60));
    assert_eq!(earlier_session["is_current"].as_bool(), Some(false));

    let unknown = json_document(&config, AS_OF, 0, 1, &[], None, never_ran());
    assert!(unknown["session"]["fingerprint"].is_null());
    assert!(unknown["session"]["began"].is_null());
}

#[test]
fn json_says_when_the_sampler_is_running() {
    let document = json_document(&Config::default(), AS_OF, 0, 1, &[], None, running());

    assert_eq!(
        document["sampler"],
        serde_json::json!({
            "running": true,
            "stopped": null,
        })
    );
}

#[test]
fn json_says_nothing_stopped_on_a_machine_that_never_started_one() {
    // Not running, and no reason to give: a fresh install where `--enable` was
    // never run. Reporting a stop here would tell a consumer a run died where
    // none ever ran, and reading `running` from the absence of a stop would
    // claim one is live.
    let document = json_document(&Config::default(), AS_OF, 0, 1, &[], None, never_ran());

    assert_eq!(
        document["sampler"],
        serde_json::json!({
            "running": false,
            "stopped": null,
        })
    );
}

#[test]
fn json_carries_the_complete_sampler_stop() {
    let stop = SamplerStop {
        reason: StopReason::Failed,
        at: Some(AS_OF - 3 * 60),
        detail: Some("history write failed".to_string()),
    };
    let document = json_document(&Config::default(), AS_OF, 0, 1, &[], None, stopped(&stop));

    assert_eq!(
        document["sampler"],
        serde_json::json!({
            "running": false,
            "stopped": {
                "reason": "failed",
                "at": AS_OF - 3 * 60,
                "detail": "history write failed",
            },
        })
    );
}
