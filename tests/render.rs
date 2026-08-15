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

use pulse::config::Config;
use pulse::model::{AgentState, Level, WorkspaceActivity};
use pulse::render::{
    badge, display_width, duration, pane, pane_geometry, sparkline, state_glyph, GAP, QUIET, RAMP,
};

fn activity(
    label: &str,
    series: Vec<Option<Level>>,
    state: AgentState,
    state_for: Option<u64>,
    agent_count: usize,
) -> WorkspaceActivity {
    WorkspaceActivity {
        workspace_id: format!("w-{label}"),
        label: label.to_string(),
        series,
        state,
        state_for,
        agent_count,
    }
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
    let config = Config::default();
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
    let config = Config::default();
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
fn the_badge_shows_the_newest_columns_when_the_series_is_longer() {
    let config = config_with_columns(3);
    let rendered = badge(
        &activity(
            "long",
            levels(&[1, 1, 1, 1, 1, 6, 7, 8]),
            AgentState::Working,
            Some(5),
            1,
        ),
        &config,
    );
    // The badge answers "recently", so it keeps the tail, not the head.
    assert_eq!(rendered, "▆▇█>");
}

#[test]
fn the_badge_honours_the_configured_column_count() {
    let series: Vec<Option<Level>> = levels(&[4; 40]);
    for columns in [1usize, 2, 8, 16, 64] {
        let config = config_with_columns(columns);
        let rendered = badge(
            &activity("wide", series.clone(), AgentState::Idle, Some(1), 1),
            &config,
        );
        assert_eq!(
            rendered.chars().count(),
            columns.min(series.len()) + 1,
            "{columns} columns plus one state glyph"
        );
    }
}

#[test]
fn a_short_series_is_not_padded_out_to_the_column_count() {
    // Padding would invent columns we never had data for. Drawing fewer is
    // honest; the store hands every workspace the same column count anyway.
    let config = Config::default();
    let rendered = badge(
        &activity("young", levels(&[3]), AgentState::Working, Some(2), 1),
        &config,
    );
    assert_eq!(rendered, "▃>");
}

#[test]
fn a_badge_window_that_is_all_gaps_still_reports_the_current_state() {
    // History exists, but nothing recent. Blanking the badge here would hide
    // that we stopped watching a workspace that is blocked right now.
    let config = config_with_columns(4);
    let mut series = levels(&[7, 7]);
    series.extend(vec![None; 4]);
    let rendered = badge(
        &activity("stale", series, AgentState::Blocked, Some(4_000), 1),
        &config,
    );
    assert_eq!(rendered, format!("{}!", GAP.to_string().repeat(4)));
}

#[test]
fn the_badge_fits_its_sidebar_budget_at_the_default_configuration() {
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
    pane(&rows, &Config::default(), 1_723_000_000)
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
    // A gap is a space, so a row that begins unobserved must not look like one
    // whose label was padded further.
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
fn a_pane_with_nothing_recorded_says_so_rather_than_drawing_an_empty_table() {
    let rendered = pane(&[], &Config::default(), 1_723_000_000);
    assert!(rendered.to_lowercase().contains("no workspace history"));
    assert!(rendered.contains("--enable"));
    // An empty table would read as "we looked, and every workspace was quiet".
    assert!(!rendered.contains("workspace  "));
}

#[test]
fn the_pane_explains_that_a_blank_column_means_unobserved() {
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[1, 2]),
        AgentState::Idle,
        Some(5),
        1,
    )]);
    let lower = rendered.to_lowercase();
    assert!(lower.contains("legend"));
    assert!(
        lower.contains("blank not observed"),
        "a gap is invisible without a key that names it:\n{rendered}"
    );
    assert!(lower.contains("nothing happened"));
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
    let rendered = sample_pane(vec![activity(
        "web",
        levels(&[1]),
        AgentState::Unknown,
        None,
        0,
    )]);
    assert!(rendered.contains('?'));
    assert!(!rendered.contains("0s"));
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
        activity("", vec![], AgentState::Unknown, None, 0),
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
    ];
    for as_of in [0u64, 1, 86_399, u64::MAX] {
        let rendered = pane(&rows, &config, as_of);
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
