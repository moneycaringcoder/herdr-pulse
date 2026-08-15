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

use crate::config::Config;
use crate::model::{AgentState, Level, WorkspaceActivity};
use crate::Result;

/// Eight steps of block element, indexed by [`Level`] 1..=8. Level 0 is
/// [`QUIET`], which is deliberately not `▁`: a one-pixel bar reads as a small
/// amount of activity rather than as none.
pub const RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Observed, and nothing happened.
pub const QUIET: char = '·';

/// Not observed. The sampler was not running for this column.
pub const GAP: char = ' ';

/// One glyph per state, for the badge and the pane.
pub fn state_glyph(_state: AgentState) -> char {
    todo!("presenter")
}

/// Renders a series as a sparkline of exactly `series.len()` characters.
///
/// Never panics, whatever the input. A [`Level`] above [`Level::MAX`] is clamped
/// rather than indexing out of bounds.
pub fn sparkline(_series: &[Option<Level>]) -> String {
    todo!("presenter")
}

/// The sidebar badge for one workspace: a sparkline plus the current state.
///
/// Budget is roughly eight display columns plus a glyph. Returns the **empty
/// string** when there is nothing worth showing, which the daemon treats as
/// "clear the token" rather than as an empty badge — so an untracked workspace
/// does not occupy a sidebar row with blanks.
pub fn badge(_activity: &WorkspaceActivity, _config: &Config) -> String {
    todo!("presenter")
}

/// A human duration in at most four characters: `12s`, `4m`, `1h20`, `3d`.
/// `None` renders as `?`, because "we do not know how long" is a real answer.
pub fn duration(_seconds: Option<u64>) -> String {
    todo!("presenter")
}

/// The full activity pane: every tracked workspace, its sparkline, its current
/// state and how long that state has lasted.
///
/// Columns must line up regardless of label width or of multi-byte glyphs in the
/// sparkline — pad by display width, not by byte length.
pub fn pane(_activity: &[WorkspaceActivity], _config: &Config, _as_of: u64) -> String {
    todo!("presenter")
}

/// `--once`: print the pane once and exit.
pub fn run_once(_config: &Config) -> Result<()> {
    todo!("presenter")
}

/// `--json`: the recorded history as machine-readable JSON.
///
/// A gap must be representable and distinguishable from a zero — `null` in the
/// series array, not `0`.
pub fn run_json(_config: &Config) -> Result<()> {
    todo!("presenter")
}

/// `--watch`: the pane, redrawn on an interval, reading the history the daemon
/// writes. Must degrade with a clear message when no sampler is running, rather
/// than showing an empty pane that looks like a quiet session.
pub fn run_watch(_config: &Config) -> Result<()> {
    todo!("presenter")
}
