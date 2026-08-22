//! Sparkline rendering, the badge, and the activity pane.
//!
//! Owned by the presenter. `model.rs`, `config.rs` and `history.rs` are the
//! contract and must not be edited here.
//!
//! # Cosmetic output is the product
//!
//! Users judge this plugin by one row in a narrow sidebar. Alignment, column
//! counts and the gap glyph being right is not polish, it is the whole feature.
//!
//! # The sparkline
//!
//! [`sparkline`] is a pure function over a series and deserves the bulk of the
//! tests: an empty series, one column, all-zero, all-max, a series longer than
//! the display width, a series that is entirely gaps, and gaps interleaved with
//! data.
//!
//! A `None` column is a **gap** — the sampler was not running then — and must
//! render as [`GAP`], never as the empty-level block. "We were not watching" and
//! "nothing happened" are different facts, and conflating them is the class of
//! bug this plugin was written to avoid committing.

use std::io::Write;

use serde_json::{json, Value};

use crate::config::{Config, WEEK_BUCKETS_PER_COLUMN, WEEK_BUCKET_SECONDS, WEEK_COLUMNS};
use crate::model::{AgentActivity, AgentState, Level, SessionMark, WorkspaceActivity};
use crate::Result;
use crate::{daemon, herdr, history};

/// Eight steps of block element, indexed by [`Level`] 1..=8. Level 0 is
/// [`QUIET`], which is deliberately not `▁`: a one-pixel bar reads as a small
/// amount of activity rather than as none.
pub const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Observed, and nothing happened.
pub const QUIET: char = '·';

/// Not observed. The sampler was not running for this column.
///
/// A broken line rather than a blank, and that is not a cosmetic preference —
/// a blank is actively unsafe here. Verified against a live herdr 0.8.0 server:
/// it **trims leading and trailing whitespace from a badge token's value**, and
/// treats an all-whitespace value as a *delete*. With a space, three things went
/// silently wrong:
///
///   * trailing gaps — the newest columns, the ones the badge exists to show —
///     were stripped, so the sparkline stopped being aligned to "now" and older
///     activity read as current;
///   * leading gaps were stripped, so badges had ragged widths down the sidebar;
///   * a series that was *entirely* gaps became an empty string, which herdr
///     deleted, so a workspace nobody had been watching lost its badge instead
///     of showing that nobody had been watching.
///
/// That last one is the exact failure this plugin is built to avoid: the moment
/// the record is least trustworthy is the moment the warning disappears. Interior
/// spaces do survive the round trip, which is why the bug only ever showed up at
/// the ends.
pub const GAP: char = '╌';

/// Sparkline columns in the activity pane. Wider than the badge because the pane
/// is an overlay rather than a sidebar cell, and narrow enough that a row still
/// fits an 80-column terminal beside a label, a state and a duration.
pub const PANE_COLUMNS: usize = 32;

/// Display columns a workspace label may occupy in the pane before it is
/// elided. One pathological label must not widen every other row.
const MAX_LABEL_WIDTH: usize = 28;

/// Blank columns between two pane columns.
const COLUMN_GAP: &str = "  ";

/// Clear the screen and home the cursor, for `--watch`.
const CLEAR: &str = "\x1b[2J\x1b[H";

/// One glyph per state, for the badge and the pane.
///
/// Deliberately ASCII. The block ramp is the only Unicode we have live evidence
/// for surviving the badge round trip at a known display width, and a state
/// glyph that a terminal decided to render double-width would shift the sidebar
/// row it sits in — the one thing this module exists to get right. Distinctness
/// at a glance matters more here than prettiness.
pub fn state_glyph(state: AgentState) -> char {
    match state {
        AgentState::Blocked => '!',
        AgentState::Working => '>',
        AgentState::Idle => '-',
        AgentState::Done => '=',
        AgentState::Unknown => '?',
    }
}

/// Renders a series as a sparkline of exactly `series.len()` characters.
///
/// Never panics, whatever the input. A [`Level`] above [`Level::MAX`] is clamped
/// rather than indexing out of bounds.
pub fn sparkline(series: &[Option<Level>]) -> String {
    series
        .iter()
        .map(|slot| match slot {
            None => GAP,
            Some(level) if level.is_quiet() => QUIET,
            // Clamped, not trusted: `Level` is a public tuple struct, so a value
            // above `Level::MAX` can reach us without ever passing through
            // `Level::new`. Indexing `RAMP` with it would panic in the one code
            // path a user sees on every refresh.
            Some(level) => RAMP[level.0.min(Level::MAX) as usize - 1],
        })
        .collect()
}

/// The sidebar badge for one workspace: a sparkline plus the current state.
///
/// Budget is roughly eight display columns plus a glyph.
///
/// # The rule
///
/// Returns the **empty string** when the series contains no observation at all,
/// which the daemon reads as "clear the token" rather than as a badge to draw.
/// That is the whole rule, and it is deliberately narrow: a workspace that is
/// merely *quiet* keeps its badge, because a flat row of `·` beside a burst that
/// ended ten minutes ago is the most useful sentence this plugin says.
///
/// # What this comment used to claim
///
/// It said "nothing worth showing" was judged over the whole series rather than
/// over the window being drawn, so that a workspace unobserved since this
/// morning kept its badge. That distinction never existed. `daemon::cycle` asks
/// the store for exactly `config.badge_columns` columns, so the series *is* the
/// window, `start` is always 0, and no truncation happens in production. The
/// only tests that demonstrated the difference built a series the store cannot
/// produce, so the suite protected the disagreement instead of catching it.
///
/// The empty-string path is therefore a floor rather than a routine outcome: the
/// store only builds a series for a workspace it has observed at least once, and
/// a live sampler observes every workspace every cycle, so the newest column is
/// a gap only when the store is asked for a window entirely older than anything
/// it recorded.
///
/// The truncation below stays as a **width guard** — the badge must never exceed
/// the configured sidebar budget, whoever calls it — and not as a semantic. It
/// is the reason the width invariant is a property of this function rather than
/// a convention between two modules.
pub fn badge(activity: &WorkspaceActivity, config: &Config) -> String {
    if activity.series.iter().all(Option::is_none) {
        return String::new();
    }

    let columns = config.badge_columns.max(1);
    let start = activity.series.len().saturating_sub(columns);
    let mut out = sparkline(&activity.series[start..]);
    out.push(state_glyph(activity.state));
    out
}

/// How far behind the last observation a workspace may be before the pane stops
/// stating its state in the present tense.
///
/// Three sampling intervals, which is the same window [`Config::ttl_ms`] gives a
/// badge and for the same reason: one missed cycle is ordinary jitter — a slow
/// snapshot, a busy machine, a save that took longer than usual — and must not
/// flip every row to the past tense, while a sampler that has genuinely stopped
/// crosses the line within seconds.
///
/// Derived from `interval` rather than fixed, because the only meaningful unit
/// of lateness here is "sampling cycles missed". A user sampling once a minute
/// would otherwise be told their whole session was stale between two perfectly
/// healthy cycles.
pub fn staleness_tolerance(config: &Config) -> u64 {
    config.interval.as_secs().saturating_mul(3).max(1)
}

/// A human duration in at most four characters: `12s`, `4m`, `1h20`, `3d`.
/// `None` renders as `?`, because "we do not know how long" is a real answer.
pub fn duration(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "?".to_string();
    };
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 10 {
        // The minutes are zero-padded so the field is always four characters
        // wide: `1h05`, never the ambiguous `1h5`.
        return format!("{hours}h{:02}", minutes % 60);
    }
    if hours < 24 {
        // Two-digit hours have spent the budget; the minutes go.
        return format!("{hours}h");
    }
    let days = hours / 24;
    // Everything here is division, so a garbage `state_since` — a clock that
    // jumped, a value near `u64::MAX` — saturates into a legible answer instead
    // of overflowing or printing a fifteen-digit number into a four-wide column.
    if days < 100 {
        format!("{days}d")
    } else {
        ">99d".to_string()
    }
}

/// Explains why there is no live sampler, or returns `None` when there is
/// nothing recorded to explain.
///
/// The stop time is measured against `as_of` rather than the wall clock so the
/// pane, the JSON document and this line all describe the same instant.
///
/// A stamp *ahead* of `as_of` is dropped rather than saturated. The marker
/// outlives reboots, and an RTC read as local time or an NTP step easily puts it
/// in the future; saturating would print "0s ago", which is both invented and
/// the most reassuring value in the range — it reads as "you just did this" to
/// the one reader who is deciding how much of an empty sparkline to believe. No
/// age at all is what the module already prints for an age it cannot establish.
pub fn sampler_stop_message(stop: Option<&daemon::SamplerStop>, as_of: u64) -> Option<String> {
    let stop = stop?;
    let age = stop
        .at
        .filter(|at| *at <= as_of)
        .map(|at| format!(" {} ago", duration(Some(as_of - at))))
        .unwrap_or_default();
    let reason = match stop.reason {
        daemon::StopReason::Disabled => format!("disabled{age}"),
        daemon::StopReason::Terminated => format!("terminated{age}"),
        daemon::StopReason::Failed => {
            let detail = stop
                .detail
                .as_deref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            format!("the last run ended unexpectedly{age}{detail}")
        }
        daemon::StopReason::Unknown => {
            format!("the last run stopped for an unknown reason{age}")
        }
    };
    Some(format!(
        "no sampler is running — {reason}; nothing since then is recorded."
    ))
}

/// What a report should say about the sampler.
///
/// The two halves are separate because they are separate questions with
/// different evidence behind them: whether one is live now, and why the last one
/// stopped. A machine that has never started a sampler answers the first and has
/// nothing to say about the second, so deriving either from the other would
/// either claim a run died or claim one is running.
#[derive(Clone, Copy)]
pub struct SamplerState<'a> {
    pub running: bool,
    pub stop: Option<&'a daemon::SamplerStop>,
}

#[derive(Clone, Copy)]
struct PaneView {
    name: Option<&'static str>,
    columns: usize,
    seconds_per_column: u64,
}

impl PaneView {
    fn fine(config: &Config) -> Self {
        let (columns, buckets_per_column) = pane_geometry(config);
        Self {
            name: None,
            columns,
            seconds_per_column: (buckets_per_column as u64).saturating_mul(config.bucket_seconds),
        }
    }

    fn week() -> Self {
        Self {
            name: Some("week view"),
            columns: WEEK_COLUMNS,
            seconds_per_column: (WEEK_BUCKETS_PER_COLUMN as u64)
                .saturating_mul(WEEK_BUCKET_SECONDS),
        }
    }
}

/// The full activity pane: every tracked workspace, its sparkline, the state we
/// last saw it in, and when we saw it.
///
/// Columns must line up regardless of label width or of multi-byte glyphs in the
/// sparkline — pad by display width, not by byte length.
///
/// The `session` column attributes each independently recorded series. A row
/// from an earlier herdr process remains real history and keeps every observed
/// sparkline bucket; only its provenance is marked as different.
///
/// # Tense
///
/// Every state in here is a *past observation*. Most of the time the last
/// observation is seconds old and reading it as the present is fair, so the row
/// says `> working`. Once the last observation is older than
/// [`staleness_tolerance`], it is not fair, and the row says `> was working`
/// with a `seen` column giving the age.
///
/// Without that split the pane contradicts itself: a row whose sparkline is
/// nothing but gap glyphs — "nobody looked for five hours" — used to sit beside
/// the words `working  5h00`, which asserts the opposite about the same five
/// hours. The sparkline was the honest half.
pub fn pane(
    activity: &[WorkspaceActivity],
    config: &Config,
    as_of: u64,
    session: Option<&SessionMark>,
) -> String {
    render_pane(activity, config, as_of, session, PaneView::fine(config))
}

fn render_pane(
    activity: &[WorkspaceActivity],
    config: &Config,
    as_of: u64,
    session: Option<&SessionMark>,
    view: PaneView,
) -> String {
    if activity.is_empty() {
        // Not an empty table. An empty table looks like a quiet session, and a
        // quiet session is the one answer we must never give by accident.
        let header = match view.name {
            Some(name) => format!("pulse — {name} — {}", clock(as_of)),
            None => format!("pulse — {}", clock(as_of)),
        };
        return format!(
            "{header}\n\nNo workspace history recorded yet.\n\
             Start the sampler with `pulse --enable`; activity appears after the first bucket.\n"
        );
    }

    let tolerance = staleness_tolerance(config);
    let mut any_stale = false;
    let explain_sessions = activity
        .iter()
        .any(|workspace| workspace.session.is_none() || !workspace.is_session(session));
    let explain_agents = activity
        .iter()
        .any(|workspace| !workspace.agents.is_empty());

    let mut rows = vec![vec![
        "workspace".to_string(),
        "activity".to_string(),
        "session".to_string(),
        "state".to_string(),
        "blocked".to_string(),
        "for".to_string(),
        "seen".to_string(),
        "agents".to_string(),
    ]];
    for workspace in activity {
        let current = workspace.is_current(as_of, tolerance);
        any_stale |= !current;
        rows.push(vec![
            clean_label(&workspace.label),
            // Bracketed so the series has visible ends. [`GAP`] is a printing
            // character rather than a blank, so this is no longer load-bearing
            // for legibility, but the delimiters still make the column width
            // obvious when every glyph in a row is the same.
            format!("[{}]", sparkline(&workspace.series)),
            session_cell(workspace, session),
            // The tense is the whole point. `was` costs four columns and is the
            // difference between reporting a fact and inventing one.
            if current {
                format!("{} {}", state_glyph(workspace.state), workspace.state)
            } else {
                format!("{} was {}", state_glyph(workspace.state), workspace.state)
            },
            // A zero backed by watching is an observation; the same zero with no
            // watched time is no observation at all and must remain unknown.
            duration((workspace.watched_seconds > 0).then_some(workspace.blocked_seconds)),
            // Measured to `last_seen` by the store, so this is the duration we
            // actually observed rather than one extrapolated to now.
            duration(workspace.state_for),
            duration(workspace.observed_ago(as_of)),
            workspace.agent_count.to_string(),
        ]);
        // Agent rows share the workspace table rather than building a second,
        // hand-padded layout. Session provenance belongs to the aggregate, the
        // per-agent ring has no `for`, and the ragged row omits its agent count.
        let mut agents: Vec<&AgentActivity> = workspace.agents.iter().collect();
        agents.sort_by(|left, right| {
            right
                .last_seen
                .cmp(&left.last_seen)
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });
        for agent in agents {
            let current = as_of.saturating_sub(agent.last_seen) <= tolerance;
            any_stale |= !current;
            // The pane id goes in the label whenever the program does not
            // identify the row on its own. Three panes running `claude` in one
            // workspace is the case this feature exists for, and three rows
            // labelled `claude` answer "which of them was busy" with a sparkline
            // and nothing to tie it back to a pane.
            let unique_program = agent.program.as_deref().is_some_and(|program| {
                workspace
                    .agents
                    .iter()
                    .filter(|other| other.program.as_deref() == Some(program))
                    .count()
                    == 1
            });
            let label = match (&agent.program, unique_program) {
                (Some(program), true) => program.clone(),
                (Some(program), false) => format!("{program} {}", agent.pane_id),
                (None, _) => agent.pane_id.clone(),
            };
            rows.push(vec![
                format!("  {}", clean_label(&label)),
                format!("[{}]", sparkline(&agent.series)),
                String::new(),
                if current {
                    format!("{} {}", state_glyph(agent.state), agent.state)
                } else {
                    format!("{} was {}", state_glyph(agent.state), agent.state)
                },
                duration((agent.watched_seconds > 0).then_some(agent.blocked_seconds)),
                String::new(),
                duration(Some(as_of.saturating_sub(agent.last_seen))),
            ]);
        }
    }

    let mut out = match view.name {
        Some(name) => format!(
            "pulse — {name} — {} workspace{} — {}\n\n",
            activity.len(),
            if activity.len() == 1 { "" } else { "s" },
            clock(as_of)
        ),
        None => format!(
            "pulse — {} workspace{} — {}\n\n",
            activity.len(),
            if activity.len() == 1 { "" } else { "s" },
            clock(as_of)
        ),
    };
    out.push_str(&table(&rows));
    out.push('\n');
    out.push_str(&legend(
        activity,
        any_stale,
        explain_sessions,
        explain_agents,
        session,
        view,
    ));
    out
}

/// The week pane uses the same table and row metadata as [`pane`], changing
/// only the history series and the scale stated in the header and legend.
pub fn week_pane(
    activity: &[WorkspaceActivity],
    config: &Config,
    as_of: u64,
    session: Option<&SessionMark>,
) -> String {
    let mut activity = activity.to_vec();
    for workspace in &mut activity {
        std::mem::swap(&mut workspace.series, &mut workspace.week);
        // The blocked figure travels with the series it describes. Left behind,
        // the week row would print the fine ring's last few hours beside a
        // seven-day sparkline, under a legend that says the figure covers the
        // time watched in *this* row.
        std::mem::swap(
            &mut workspace.blocked_seconds,
            &mut workspace.week_blocked_seconds,
        );
        std::mem::swap(
            &mut workspace.watched_seconds,
            &mut workspace.week_watched_seconds,
        );
        // There is no coarse per-agent ring. Drawing the fine ring under a week
        // row would put an afternoon beside an axis labelled as seven days, so
        // the week view remains aggregate-only.
        workspace.agents.clear();
    }
    render_pane(&activity, config, as_of, session, PaneView::week())
}

/// The live herdr identity, when both locating and fingerprinting its socket
/// succeed. Reporting commands still render saved history when there is no
/// socket, so a path lookup failure is provenance we do not know, not a command
/// failure.
fn live_session() -> Option<SessionMark> {
    herdr::socket_path()
        .ok()
        .and_then(|socket| herdr::session_mark(&socket))
}

/// `--once`: print the pane once and exit.
pub fn run_once(config: &Config) -> Result<()> {
    let as_of = crate::now_unix();
    let session = live_session();
    let (columns, buckets_per_column) = pane_geometry(config);
    let activity = history::load(config).activity(as_of, columns, buckets_per_column, config);
    print!("{}", pane(&activity, config, as_of, session.as_ref()));

    // A stopped sampler can leave either an empty report or a convincing stretch
    // of saved history. Say what stopped in both cases. On stderr, so
    // `pulse --once > report.txt` still captures only the report.
    if let Some(message) = sampler_stop_message(current_sampler_stop().as_ref(), as_of) {
        eprintln!("pulse: {message}");
    }
    Ok(())
}

/// `--week`: print the week pane once and exit.
pub fn run_week(config: &Config) -> Result<()> {
    let as_of = crate::now_unix();
    let session = live_session();
    let (columns, buckets_per_column) = pane_geometry(config);
    let activity = history::load(config).activity(as_of, columns, buckets_per_column, config);
    print!("{}", week_pane(&activity, config, as_of, session.as_ref()));

    if let Some(message) = sampler_stop_message(current_sampler_stop().as_ref(), as_of) {
        eprintln!("pulse: {message}");
    }
    Ok(())
}

/// `--json`: the recorded history as machine-readable JSON.
///
/// A gap must be representable and distinguishable from a zero — `null` in
/// either history array, not `0`. Consumers need both arrays because the fine
/// series answers "what happened this afternoon", while the coarse week series
/// answers "did anything happen yesterday"; neither can be derived from the
/// other.
///
/// `blocked_seconds` is estimated only over buckets the sampler observed, so it
/// is paired with `watched_seconds` to expose the evidence behind that estimate.
/// A consumer must not divide by the row's wall-clock width: gaps contribute to
/// neither value, and treating them as unblocked time would turn "not watched"
/// into "watched and quiet".
///
/// `state` carries the same freshness problem the pane solves with a tense, and
/// a consumer cannot see a tense. So every workspace also carries `last_seen`,
/// `observed_ago_seconds` and `state_is_current`, and the document carries the
/// `staleness_tolerance_seconds` those were judged against. Without them a tool
/// reading `"state":"working"` has no way to discover that nobody has looked in
/// five hours, and would be right to treat it as current.
///
/// Per-agent series cannot be recovered by splitting the workspace series:
/// the aggregate has already combined simultaneous agents into one level and
/// discarded which pane contributed each observation. The nested `agents`
/// arrays preserve both that identity and the gaps before an agent appeared.
///
/// A series of `null` buckets only says nobody was watching. The top-level
/// `sampler` field is the only field that says whether that was on purpose or
/// because the last run ended.
pub fn run_json(config: &Config) -> Result<()> {
    let as_of = crate::now_unix();
    let session = live_session();
    let stop = current_sampler_stop();
    let (columns, buckets_per_column) = pane_geometry(config);
    let activity = history::load(config).activity(as_of, columns, buckets_per_column, config);
    let document = json_document(
        config,
        as_of,
        columns,
        buckets_per_column,
        &activity,
        session.as_ref(),
        SamplerState {
            running: sampler_is_running(),
            stop: stop.as_ref(),
        },
    );
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

fn json_series(series: &[Option<Level>]) -> Vec<Value> {
    series
        .iter()
        .map(|slot| match slot {
            Some(level) => json!(level.0.min(Level::MAX)),
            None => Value::Null,
        })
        .collect()
}

/// Builds the `--json` document from already-loaded activity.
///
/// Pulled out of [`run_json`] as a pure function so the gap/quiet distinction —
/// `null` in `series` for a gap, `0` for an observed-but-idle bucket — is
/// checkable directly against a hand-built [`WorkspaceActivity`], without going
/// through the history store or a subprocess.
///
/// Session provenance prevents a consumer from joining two entries for one
/// checkout into one interrupted series. They were recorded by different herdr
/// sessions, and merging them would state a continuity no sampler observed.
pub fn json_document(
    config: &Config,
    as_of: u64,
    columns: usize,
    buckets_per_column: usize,
    activity: &[WorkspaceActivity],
    session: Option<&SessionMark>,
    sampler: SamplerState<'_>,
) -> Value {
    let tolerance = staleness_tolerance(config);

    // `running` and `stopped` answer different questions and are read from
    // different places. A machine where no sampler was ever started is not
    // running and has no stop to report, and reading one from the other would
    // either claim a run died or claim one is live.
    let sampler = json!({
        "running": sampler.running,
        "stopped": match sampler.stop {
            Some(stop) => json!({
                "reason": stop.reason.as_str(),
                "at": stop.at,
                "detail": stop.detail.as_deref(),
            }),
            None => Value::Null,
        },
    });

    let workspaces: Vec<Value> = activity
        .iter()
        .map(|workspace| {
            json!({
                "workspace_id": workspace.workspace_id,
                "label": workspace.label,
                "session": {
                    "fingerprint": workspace.session.as_deref(),
                    "began": workspace.session_began,
                    // `null`, not `false`, when there is no live session to
                    // compare against: "recorded by a session that has ended"
                    // and "pulse could not establish which session is running"
                    // are different facts, and `false` asserts the first.
                    "is_current": session.map(|_| workspace.is_session(session)),
                },
                "state": workspace.state.as_str(),
                // Measured to `last_seen`, not to `as_of`: the duration we
                // observed, never one extrapolated across an outage.
                "state_for_seconds": workspace.state_for,
                // The three fields that make `state` falsifiable.
                "last_seen": workspace.last_seen,
                "observed_ago_seconds": workspace.observed_ago(as_of),
                "state_is_current": workspace.is_current(as_of, tolerance),
                "agent_count": workspace.agent_count,
                // These cannot be derived from the aggregate series: once
                // several panes have contributed to one level, their identities
                // and their individual absence-before-appearance gaps are gone.
                "agents": workspace
                    .agents
                    .iter()
                    .map(|agent| json!({
                        "pane_id": agent.pane_id,
                        "program": agent.program,
                        "state": agent.state.as_str(),
                        "series": json_series(&agent.series),
                        "last_seen": agent.last_seen,
                        // The same two fields the workspace object carries, for
                        // the same reason: a consumer reading `"state":"working"`
                        // has no way to see that nobody has looked in five hours,
                        // and the pane already draws `was working` for exactly
                        // these rows.
                        "observed_ago_seconds": as_of.saturating_sub(agent.last_seen),
                        "state_is_current": as_of.saturating_sub(agent.last_seen) <= tolerance,
                        "blocked_seconds": agent.blocked_seconds,
                        "watched_seconds": agent.watched_seconds,
                    }))
                    .collect::<Vec<Value>>(),
                // Both describe the fine-series window. `watched_seconds` is
                // the denominator that keeps gaps out of the blocked estimate;
                // the row's wall-clock width includes gaps and is not evidence.
                "blocked_seconds": workspace.blocked_seconds,
                "watched_seconds": workspace.watched_seconds,
                // And the same pair over the week series, because the two rings
                // cover different stretches and a figure from one says nothing
                // about the other.
                "week_blocked_seconds": workspace.week_blocked_seconds,
                "week_watched_seconds": workspace.week_watched_seconds,
                // `null` is a gap and `0` is an observed quiet bucket. A
                // consumer that flattens the two gets the same wrong answer the
                // glyphs exist to prevent, so the distinction is carried in the
                // JSON type rather than in a sentinel number.
                "series": json_series(&workspace.series),
                "week": json_series(&workspace.week),
                // The rendered form too, so a bug report can show what the user
                // saw without asking them to screenshot a sidebar.
                "sparkline": sparkline(&workspace.series),
            })
        })
        .collect();

    json!({
        "as_of": as_of,
        "session": {
            "fingerprint": session.map(|mark| mark.fingerprint.as_str()),
            "began": session.map(|mark| mark.began),
        },
        "sampler": sampler,
        "bucket_seconds": config.bucket_seconds,
        "columns": columns,
        "buckets_per_column": buckets_per_column,
        "seconds_per_column": (buckets_per_column as u64).saturating_mul(config.bucket_seconds),
        // The week array needs its own three, for the same reason the fine one
        // does: a column is not a bucket. Without the column width a consumer
        // that reasonably mirrors the fine naming reads `week_bucket_seconds x
        // week_columns` as the row span and mislabels its axis by a factor of
        // six on every draw.
        "week_bucket_seconds": WEEK_BUCKET_SECONDS,
        "week_columns": WEEK_COLUMNS,
        "week_buckets_per_column": WEEK_BUCKETS_PER_COLUMN,
        "week_seconds_per_column": (WEEK_BUCKETS_PER_COLUMN as u64)
            .saturating_mul(WEEK_BUCKET_SECONDS),
        "staleness_tolerance_seconds": tolerance,
        "level_max": Level::MAX,
        "workspaces": workspaces,
    })
}

/// `--watch`: the pane, redrawn on an interval, reading the history the daemon
/// writes. Must degrade with a clear message when no sampler is running, rather
/// than showing an empty pane that looks like a quiet session.
pub fn run_watch(config: &Config) -> Result<()> {
    if !sampler_is_running() {
        return Err(no_sampler_error(current_sampler_stop().as_ref(), crate::now_unix()).into());
    }

    let (columns, buckets_per_column) = pane_geometry(config);
    let mut out = std::io::stdout();
    loop {
        // Re-checked every cycle, not just at startup. A sampler that dies under
        // a running watch would otherwise leave the last frame on screen for
        // hours, ageing into a confident lie.
        //
        // Liveness is the gate rather than the stop report, because a machine
        // that never started a sampler has nothing to report and still has
        // nothing to watch.
        if !sampler_is_running() {
            write!(out, "{CLEAR}")?;
            out.flush()?;
            return Err(
                no_sampler_error(current_sampler_stop().as_ref(), crate::now_unix()).into(),
            );
        }

        let as_of = crate::now_unix();
        // The bound socket can be replaced while a watch is open, so provenance
        // is sampled with each frame rather than frozen at startup.
        let session = live_session();
        let activity = history::load(config).activity(as_of, columns, buckets_per_column, config);
        write!(
            out,
            "{CLEAR}{}",
            pane(&activity, config, as_of, session.as_ref())
        )?;
        writeln!(
            out,
            "\nrefreshing every {} — ctrl-c to stop",
            duration(Some(config.interval.as_secs()))
        )?;
        out.flush()?;
        std::thread::sleep(config.interval);
    }
}

/// The `--watch` failure message: why there is nothing to watch.
///
/// The reason is optional because "no sampler is running" is answerable on a
/// machine that has never started one, while "why did the last one stop" is not.
/// Without a recorded reason the message says only what is known.
fn no_sampler_error(stop: Option<&daemon::SamplerStop>, as_of: u64) -> String {
    let message = sampler_stop_message(stop, as_of)
        .unwrap_or_else(|| "no sampler is running, so there is nothing live to watch.".to_string());
    format!(
        "{message} Start it with `pulse --enable`, or use `pulse --once` to read what was recorded earlier"
    )
}

/// The recorded reason the sampler is not running, or `None` when there is
/// nothing to explain.
///
/// A thin pass-through, and deliberately nothing more. [`daemon::stop_report`]
/// already answers all three cases — a live daemon, a recorded stop, and a run
/// that died without a word — and its third answer is `None` for a machine where
/// no sampler was ever started. Inventing an "unknown stop" here would put "the
/// last run stopped for an unknown reason" under the pane on a fresh install,
/// directly beside the pane's own "start the sampler with `pulse --enable`":
/// two lines contradicting each other, and a `--json` consumer told a run died
/// where none ever ran.
fn current_sampler_stop() -> Option<daemon::SamplerStop> {
    daemon::stop_report()
}

/// Whether a sampler is live right now.
///
/// Asked separately from [`current_sampler_stop`] because they are different
/// questions: "is one running" has an answer on a machine that has never run
/// one, and "why did the last one stop" does not.
fn sampler_is_running() -> bool {
    daemon::live_pid().is_some()
}

/// How many pane columns to ask the store for, and how many buckets each one
/// aggregates.
///
/// Derived from retention rather than from the badge window: the pane is where
/// someone goes to see the whole recorded history, not the last hour of it. The
/// column count is capped by the number of buckets that exist, so a small
/// retention draws a short row rather than a long row of gaps that were never
/// recordable in the first place.
pub fn pane_geometry(config: &Config) -> (usize, usize) {
    let columns = PANE_COLUMNS.min(config.retention_buckets).max(1);
    let buckets_per_column = (config.retention_buckets / columns).max(1);
    (columns, buckets_per_column)
}

/// Time of day in UTC.
///
/// Deliberately not a date. A calendar needs civil-from-days arithmetic and a
/// timezone database to be worth printing, and the question the pane answers —
/// "how fresh is this?" — is answered by a clock.
fn clock(as_of: u64) -> String {
    let [hours, minutes, seconds] = clock_parts(as_of);
    format!("{hours:02}:{minutes:02}:{seconds:02} UTC")
}

/// Time of day components in UTC, shared by the pane header and the session
/// column so the hour and minute formats cannot drift apart.
fn clock_parts(as_of: u64) -> [u64; 3] {
    let seconds = as_of % 86_400;
    [seconds / 3_600, (seconds / 60) % 60, seconds % 60]
}

/// The minute at which a herdr session began. Seconds are deliberately omitted:
/// the column identifies a run at a glance without spending sidebar width on
/// precision the row does not need.
fn session_clock(began: u64) -> String {
    let [hours, minutes, _] = clock_parts(began);
    format!("{hours:02}:{minutes:02}")
}

/// What the `session` column says about one row.
///
/// The parentheses mean exactly one thing — "recorded by a session other than
/// the one running now" — so they may only be drawn when there *is* a known
/// session running now. With no live mark, nothing is comparable to anything:
/// the cell states when the row's own session began and claims nothing further,
/// because "an earlier session than the one running now" would be a claim about
/// a session that was never established.
fn session_cell(workspace: &WorkspaceActivity, session: Option<&SessionMark>) -> String {
    let began = |began: Option<u64>| match began {
        Some(began) => session_clock(began),
        None => "?".to_string(),
    };
    if session.is_none() {
        return began(workspace.session_began);
    }
    if workspace.is_session(session) {
        return began(session.map(|mark| mark.began));
    }
    format!("({})", began(workspace.session_began))
}

/// The key to the glyphs.
///
/// The distinction between "observed and quiet" and "not observed" is the whole
/// point of the plugin, and it is carried by two small characters that look
/// broadly similar at a glance. Without this line a user reading a half-gapped
/// sparkline would reasonably read it as quiet — the exact misreading the glyph
/// split exists to prevent — so the key is not optional decoration.
///
/// Built from the constants rather than spelled out, so a glyph can never be
/// changed without the legend following it. The previous version hard-coded the
/// word "blank", and went stale the moment [`GAP`] stopped being a space.
fn legend(
    activity: &[WorkspaceActivity],
    any_stale: bool,
    explain_sessions: bool,
    explain_agents: bool,
    session: Option<&SessionMark>,
    view: PaneView,
) -> String {
    let ramp: String = RAMP.iter().collect();
    let mut out = format!(
        "legend  {ramp} busier  |  {QUIET} observed, nothing happened  |  \
         {GAP} not observed\n"
    );
    out.push_str(
        "        for = how long the state had held when last seen  |  \
         seen = how long ago that was\n",
    );
    if activity.iter().any(|workspace| {
        workspace.watched_seconds > 0
            || workspace
                .agents
                .iter()
                .any(|agent| agent.watched_seconds > 0)
    }) {
        out.push_str(
            "        blocked = estimated from the samples that saw a blocked agent  |  \
             measured over time actually watched, not the whole row\n",
        );
    }
    if explain_agents {
        out.push_str(
            "        indented rows = single agents  |  \
             gaps before an agent appeared = not there yet, not idle\n",
        );
    }
    // Only when it applies. A reader whose rows are all fresh does not need to
    // be taught a distinction none of them makes, and a line that is always
    // there is a line nobody reads on the day it matters.
    if any_stale {
        out.push_str(
            "        \"was X\" is the last observation and nothing has been seen since — \
             not a claim about now\n",
        );
    }
    if explain_sessions {
        // Assembled from what the column actually printed. A legend that
        // explains parentheses in a pane with none, or promises a live session
        // when none could be established, teaches the reader something untrue
        // about the row in front of them.
        let mut clauses =
            vec!["session = when the herdr session that recorded the row began".to_string()];
        if session.is_some() {
            clauses.push("parentheses = an earlier session than the one running now".to_string());
        } else {
            clauses.push(
                "the session running now could not be established, so no row is marked live"
                    .to_string(),
            );
        }
        if activity
            .iter()
            .any(|workspace| workspace.session_began.is_none())
        {
            clauses.push("? = that session's start could not be established".to_string());
        }
        out.push_str(&format!("        {}\n", clauses.join("  |  ")));
    }

    // Only claim a timescale when the series we were handed actually has the
    // shape this view implies. A caller that built the activity with different
    // geometry would otherwise get a confidently mislabelled axis.
    if activity
        .iter()
        .all(|workspace| workspace.series.len() == view.columns)
    {
        out.push_str(&format!(
            "        one column = {}  |  whole row = {}\n",
            duration(Some(view.seconds_per_column)),
            duration(Some(
                view.seconds_per_column.saturating_mul(view.columns as u64)
            ))
        ));
    }
    out
}

/// Lays out rows into aligned columns, padding every cell but the last.
///
/// The last cell is left unpadded so rows carry no trailing whitespace; nothing
/// lines up to the right of it anyway.
fn table(rows: &[Vec<String>]) -> String {
    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; column_count];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
    }

    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push_str(COLUMN_GAP);
            }
            if index == last {
                out.push_str(cell);
            } else {
                out.push_str(&pad(cell, widths[index]));
            }
        }
        out.push('\n');
    }
    out
}

/// Right-pads to `width` **display columns**.
fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    for _ in display_width(text)..width {
        out.push(' ');
    }
    out
}

/// A label safe to lay out.
///
/// Control characters would move the terminal cursor and undo every column we
/// just aligned, so they are dropped rather than trusted — workspace labels come
/// from whatever the user typed, or from a directory name, and are not ours.
fn clean_label(label: &str) -> String {
    let cleaned: String = label.chars().filter(|ch| !ch.is_control()).collect();
    if cleaned.trim().is_empty() {
        return "(unnamed)".to_string();
    }
    truncate_to_width(&cleaned, MAX_LABEL_WIDTH)
}

/// Truncates to `width` display columns, never splitting a character.
fn truncate_to_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

/// Terminal cells a string occupies.
///
/// `str::len()` is bytes and `chars().count()` is code points; a terminal aligns
/// by neither. The sparkline is three bytes per glyph and a workspace label may
/// be CJK or contain a combining accent, so padding by anything but this
/// silently misaligns every column to its right.
///
/// This is an approximation of UAX #11 rather than the whole table — the crate
/// has no unicode-width dependency and this is cosmetic — but it covers what
/// actually turns up in a workspace label: the CJK and Hangul blocks, fullwidth
/// forms, emoji, and zero-width combining marks.
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    let code = ch as u32;
    // Combining marks, variation selectors and the zero-width formatting
    // characters attach to the previous cell rather than claiming one.
    if matches!(code,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x200B..=0x200F
        | 0x20D0..=0x20F0
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0xFEFF
    ) {
        return 0;
    }
    if ch.is_control() {
        return 0;
    }
    if matches!(code,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}
