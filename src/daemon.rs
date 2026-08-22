//! Sampler lifecycle: detached daemon, pid/enabled markers, TTL badge pushes,
//! and cleanup that survives being killed.
//!
//! Owned by the sampler. `model.rs`, `config.rs` and `history.rs` are the
//! contract and must not be edited here.
//!
//! # The lifecycle contract
//!
//! | verb | behaviour |
//! |---|---|
//! | `--enable`  | mark enabled **first**, no-op if a live pid exists, else spawn detached |
//! | `--disable` | mark disabled **first**, request stop, **await exit**, then sweep every current workspace over a fresh connection |
//! | `--toggle`  | disable if live, else enable |
//! | `--restore` | silent no-op unless the enabled marker is set and no daemon is live |
//!
//! Awaiting exit on `--disable` is load-bearing: the stop request only *posts*,
//! and the pid file survives until the daemon finishes clearing. An `--enable`
//! landing in that window sees a live pid, spawns nothing, and the badge never
//! returns. Bound the wait (~3 s) so disable can never hang.
//!
//! A daemon herdr spawned as a child would die with herdr, so `--enable`
//! re-execs the binary as `--daemon`, detached with `setsid()` in `pre_exec`.
//!
//! # Shutdown ordering
//!
//! The signal thread clears badges over **its own connection**, so it never
//! waits on the main loop's sleep or its in-flight round trip. That much is
//! about latency. Correctness needs one more thing: the clear must be the *last*
//! word on the wire, and a `stopping` flag read once at the top of the loop
//! cannot promise that — a signal arriving mid-cycle leaves a push in flight
//! that re-lights tokens behind the clears, and `shutdown` spends a round trip
//! per workspace, which is ample wall clock for that to happen.
//!
//! So the two threads are ordered by the `active` map's mutex. [`push`] holds it
//! across every round trip it makes, and `shutdown` takes it before it clears.
//! Only two interleavings exist: the push wins the lock, runs to completion and
//! records what it lit, and `shutdown` then clears exactly that; or `shutdown`
//! wins, and the push — which re-reads `stopping` *after* acquiring — issues
//! nothing at all. The main loop parks rather than returning, so it can never
//! start a second push either way.
//!
//! What that does **not** cover, stated plainly rather than papered over: a
//! `SIGKILL`, a clear the server rejects, and a push slow enough that
//! `--disable` gives up waiting (`STOP_TIMEOUT`) and runs its own sweep before
//! the daemon exits. In all three the only backstop is the token TTL — three
//! sampling intervals, 15 s at the default — and nothing shorter.

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::herdr::{self, Herdr};
use crate::history;
use crate::model::{SessionMark, Tone, WorkspaceActivity};
use crate::Result;

/// The stop request only posts a signal; the daemon still has to clear its
/// badges. Bounded so `--disable` can never hang on a wedged daemon.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(25);

/// The main loop wakes at least this often so a stop request is noticed promptly
/// even with a long sampling interval.
const LOOP_TICK: Duration = Duration::from_millis(250);

/// Arguments the detached child is given a copy of. It re-reads the config file
/// but never sees the user's command line.
pub const FORWARDED: [&str; 4] = [
    "--interval",
    "--bucket-seconds",
    "--retention-buckets",
    "--columns",
];

/// Why the last sampler run ended, as far as it was able to say.
///
/// A gap in the history says nobody was watching. This says why nobody was
/// watching, which is the difference between "I turned it off after lunch" and
/// "it died at 11:04 and I have been reading an empty sparkline since".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `--disable`, or a `--toggle` that landed on a live daemon. The user asked.
    Disabled,
    /// A signal from somewhere else: a reboot, an OOM killer's polite first
    /// attempt, somebody's `kill`. Distinguished from [`Self::Disabled`] by the
    /// enabled marker, which `--disable` clears *before* it signals.
    Terminated,
    /// The run ended on a panic or an error it could not continue past.
    Failed,
    /// The run stopped without leaving a word. A `SIGKILL`, a power cut, a
    /// container reaped out from under it.
    ///
    /// Never reported as anything else. "Cleanly disabled" is the flattering
    /// guess, and a user deciding how much of an empty sparkline to believe is
    /// exactly the person that guess would mislead.
    Unknown,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Terminated => "terminated",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "disabled" => Self::Disabled,
            "terminated" => Self::Terminated,
            "failed" => Self::Failed,
            // Including a reason written by a newer pulse: an unrecognised word
            // is one we cannot interpret, and interpreting it anyway is how a
            // guess gets in.
            _ => Self::Unknown,
        }
    }
}

/// What happened to the last sampler run, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplerStop {
    pub reason: StopReason,
    /// Unix seconds the run ended, when it was able to record one. `None` for a
    /// run that left no marker at all — there is nobody to have written a time.
    pub at: Option<u64>,
    /// One line of detail for [`StopReason::Failed`], such as a panic message.
    pub detail: Option<String>,
}

/// Records why this run is ending, best effort.
///
/// Best effort because the alternative is worse: an unwritable state dir must
/// not turn a clean shutdown into a hang or a crash. A marker that could not be
/// written reads back as [`StopReason::Unknown`], which is exactly what it is.
fn record_stop(reason: StopReason, detail: Option<&str>) {
    let at = crate::now_unix();
    let line = match detail {
        Some(detail) => format!("{}\n{at}\n{}\n", reason.as_str(), one_line(detail)),
        None => format!("{}\n{at}\n", reason.as_str()),
    };
    let _ = fs::write(config::stop_marker(), line);
}

/// Squeezes a detail onto one line, because the marker is line-delimited and a
/// newline in the middle of it would make the file unparseable — losing the
/// reason along with the detail.
///
/// Folded, not truncated at the first newline. `std`'s panic message is
/// *always* `panicked at <location>:\n<payload>`, so cutting at the newline
/// keeps the file and line and throws away the message every single time — a
/// marker reading `(panicked at src/daemon.rs:412:9:)` with nothing after the
/// colon, in the one place a detached daemon can still say what happened.
fn one_line(detail: &str) -> String {
    let folded = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    folded
        .chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect()
}

/// Clears the marker, so a run that is starting does not inherit the last one's
/// epitaph.
fn clear_stop() {
    let _ = fs::remove_file(config::stop_marker());
}

/// Why the sampler is not running, or `None` while one is.
///
/// The three cases, in the order they are believed:
///
/// * a live daemon — nothing to explain, and a gap in the history means herdr
///   was unreachable rather than that the sampler stopped;
/// * a marker the last run wrote as it went — the reason it recorded;
/// * no marker, but evidence that a run existed: a pid file nobody cleaned up,
///   or an enabled flag with nothing running under it. That run did not get to
///   say goodbye, and [`StopReason::Unknown`] is the honest reading.
pub fn stop_report() -> Option<SamplerStop> {
    if live_pid().is_some() {
        return None;
    }
    if let Some(stop) = read_stop_marker() {
        return Some(stop);
    }
    // `live_pid` sweeps a stale pid file as it reads, so by here the evidence of
    // an unannounced death is the enabled flag: the user asked for a sampler and
    // there is not one.
    if is_enabled() {
        return Some(SamplerStop {
            reason: StopReason::Unknown,
            at: None,
            detail: None,
        });
    }
    None
}

fn read_stop_marker() -> Option<SamplerStop> {
    let raw = fs::read_to_string(config::stop_marker()).ok()?;
    let mut lines = raw.lines();
    let reason = StopReason::parse(lines.next()?.trim());
    Some(SamplerStop {
        reason,
        // A marker with an unreadable timestamp still names a reason, and the
        // reason is the part that matters; "when" degrades to unknown on its own.
        at: lines.next().and_then(|line| line.trim().parse().ok()),
        detail: lines
            .next()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .map(str::to_string),
    })
}

/// Records a panic as the reason this run ended, then lets the default hook run.
///
/// A panicking sampler is the case a user is least equipped to diagnose: the
/// process is gone, its stderr went to `/dev/null` when it detached, and all
/// that is left is a sparkline that stops. The marker is the one place that can
/// still say what happened, so it is written from inside the hook rather than
/// after unwinding, which a panic in a detached daemon may never reach.
fn install_panic_reporter() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_stop(StopReason::Failed, Some(&panic_detail(info)));
        previous(info);
    }));
}

/// A panic as one line, message first.
///
/// `PanicHookInfo`'s own `Display` puts the location first and the payload after
/// a newline, and the detail field is capped — so relying on it spends the whole
/// budget on a file and line and truncates away the sentence that says what
/// went wrong. The payload is what a reader needs first; the location follows it
/// for whoever goes looking in the source.
fn panic_detail(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_string());
    match info.location() {
        Some(location) => format!("{payload} (at {}:{})", location.file(), location.line()),
        None => payload,
    }
}

/// Records a run that ended on something it could not continue past.
///
/// Public because the exits that reach it are not all inside this module — and
/// because a test cannot construct a `PanicHookInfo`, so this is the only seam
/// where the marker's handling of a real panic string can be pinned.
pub fn record_failure(detail: &str) {
    record_stop(StopReason::Failed, Some(detail));
}

pub fn enable(args: &[String]) -> Result<()> {
    // Parse before touching any state: a typo'd value must fail here, where the
    // user can see it, and not inside a detached child whose stderr is
    // /dev/null.
    let forwarded = forwarded_args(args)?;
    config::load_with_args(args)?;

    // Mark next. If the spawn fails, or the server hands off before we finish,
    // `--restore` still knows the user wants a daemon.
    mark_enabled(true);
    if live_pid().is_some() {
        return Ok(());
    }
    spawn_detached(&forwarded)
}

pub fn disable() -> Result<()> {
    // Mark first, so nothing that observes the markers mid-teardown concludes
    // the daemon is still wanted.
    mark_enabled(false);

    if let Some(pid) = live_pid() {
        request_stop(pid);
        // Load-bearing: the stop request only posts, and the pid file lives
        // until the daemon has finished clearing. An `--enable` landing in that
        // window would see a live pid, spawn nothing, and the badge would never
        // come back.
        if !await_exit(pid, STOP_TIMEOUT) {
            eprintln!("pulse: sampler {pid} did not exit within {STOP_TIMEOUT:?}");
        }
    }
    clear_pid_file();

    // Fresh connection, and every current workspace: the daemon may have died
    // without clearing, and it only ever tracked the workspaces it had seen.
    let mut client = Herdr::connect()?;
    sweep(&mut client)
}

pub fn toggle(args: &[String]) -> Result<()> {
    if live_pid().is_some() {
        disable()
    } else {
        enable(args)
    }
}

/// herdr startup hook. Silent no-op unless the enabled marker is set and no
/// daemon is currently live.
pub fn restore() -> Result<()> {
    if !is_enabled() || live_pid().is_some() {
        return Ok(());
    }
    // A startup hook has no user command line to forward; the child falls back
    // to the config file, which is the only durable record of the user's choices
    // anyway.
    spawn_detached(&[])
}

/// [`run`], with the outcome recorded.
///
/// The loop itself never returns `Ok`: it parks when a signal arrives and the
/// signal thread exits the process. So an `Err` here is the run ending on
/// something it could not continue past, and that is as much a reason for a gap
/// as a `kill` is.
pub fn run_daemon(config: &Config) -> Result<()> {
    let outcome = run(config);
    if let Err(err) = &outcome {
        record_stop(StopReason::Failed, Some(&err.to_string()));
    }
    outcome
}

/// The sampling loop itself, running in the foreground.
///
/// One cycle: take a snapshot, fold it into the history, persist, then push one
/// badge per workspace recorded by that snapshot's session. History is saved
/// every cycle — a SIGKILLed daemon must lose at most one interval of data, not
/// the whole session.
///
/// Every exit this function can see is recorded on the way out, so a gap in the
/// history can say why nobody was watching. The exits it cannot see — a
/// `SIGKILL`, a power cut — leave no marker, and read back as
/// [`StopReason::Unknown`] rather than as a tidy shutdown.
pub fn run(config: &Config) -> Result<()> {
    write_pid(std::process::id());
    // The last run's epitaph is not this run's. Cleared after the pid marker so
    // a reader in the window between them sees a live daemon rather than a
    // sampler that stopped for no stated reason.
    clear_stop();
    install_panic_reporter();
    // Which token name is currently lit per workspace. A tone flip has to clear
    // the old name before setting the new one, or herdr renders two badges at
    // once — the merge patch only touches names we mention.
    let active: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    spawn_signal_thread(Arc::clone(&active), Arc::clone(&stopping))?;

    // Loaded once and kept in memory: the daemon is the only writer, and
    // re-reading the file every cycle would only give us back what we just
    // wrote.
    let mut history = history::load(config);
    let mut client: Option<Herdr> = None;
    // A save failure repeats every cycle for as long as the state dir is
    // unwritable, which at a 5 s interval is a wall of identical lines.
    let mut reported_save_failure = false;

    loop {
        if stopping.load(Ordering::SeqCst) {
            // The signal thread owns shutdown from here: it clears badges over
            // its own connection and exits the process. Park rather than return,
            // so this thread can never push a badge back on top of the clear it
            // is racing.
            loop {
                std::thread::park();
            }
        }

        if client.is_none() {
            match Herdr::connect() {
                Ok(connected) => client = Some(connected),
                Err(err) => eprintln!("pulse: cannot reach herdr: {err}"),
            }
        }
        if let Some(connected) = client.as_mut() {
            if let Err(err) = cycle(
                connected,
                config,
                &mut history,
                &active,
                &stopping,
                &mut reported_save_failure,
            ) {
                eprintln!("pulse: sample failed: {err}");
                // Only a transport failure is worth redialling for; an error
                // envelope means the server is fine and answered us.
                if herdr::error_code(&*err).is_none() {
                    client = None;
                }
            }
        }

        nap(config.interval, &stopping);
    }
}

/// One cycle: snapshot, record, persist, push.
///
/// Persisting sits *before* the push and happens every cycle, not on a timer and
/// not at shutdown: a daemon that is SIGKILLed never gets to flush, and the
/// whole point of the history is that it survives that.
///
/// # What that costs, measured
///
/// A full re-serialise and `fsync` of the whole store per interval. Ten
/// workspaces encode to 131 KB, so at the default 5 s interval that is 17,280
/// rewrites and about **2.3 GB written per day**. Bounded — the store is capped
/// by construction — but not small.
///
/// It is kept anyway, because every cheaper cadence buys its bytes back with the
/// one property this file exists for. `History::record` bumps the current
/// bucket's `samples` on *every* sample, so "save only when something changed"
/// is never false in a running sampler; and saving every N cycles turns "a
/// SIGKILLed daemon loses at most one interval" into "loses up to N", which is a
/// different promise to a user reading a sparkline.
///
/// # Where the bytes actually are
///
/// Worth stating precisely, because the obvious answer is only half right.
/// `Bucket` serialises four `u16` fields unconditionally, so an *unobserved*
/// ring slot spends 53 bytes saying nothing, and a `skip_serializing_if` in
/// `history.rs` would be a large win — measured at **13.5×** (2,269 → 167
/// MB/day) on a young store whose ring is mostly empty.
///
/// But a daemon spends almost all its life in steady state, with every slot in
/// the four-hour ring observed, and there the same change measures **1.3×**
/// (2,393 → 1,895 MB/day): the fields are no longer zero, so there is nothing to
/// skip. Sparsity is the young case, not the standing one, and the standing one
/// is where the daily figure comes from.
///
/// So the lever that would actually move steady state is a compact encoding, or
/// dropping `sync_all` — which is stronger than this guarantee needs anyway,
/// since a killed *process* does not lose the page cache, only a killed *kernel*
/// does. Both live in `history.rs`, which this module does not own.
fn cycle(
    client: &mut Herdr,
    config: &Config,
    history: &mut history::History,
    active: &Mutex<HashMap<String, String>>,
    stopping: &AtomicBool,
    reported_save_failure: &mut bool,
) -> Result<()> {
    let sample = client.sample(crate::now_unix())?;
    history.record(&sample, config);

    match history::save(history, config) {
        Ok(()) => *reported_save_failure = false,
        Err(err) => {
            // Losing history is much better than stopping the sampler, so this
            // is a warning and the cycle continues. Reported once per outage
            // rather than once per cycle.
            if !*reported_save_failure {
                eprintln!("pulse: could not save history: {err}");
                *reported_save_failure = true;
            }
        }
    }

    // Exactly the geometry the badge draws: `badge_columns` columns, each
    // aggregating `buckets_per_badge_column()` buckets. The store therefore
    // hands back a series whose length *is* the badge's width, which is the
    // seam `tests/daemon_state.rs` pins — a test built on any other shape is
    // testing something the daemon cannot produce.
    let activity = history.activity(
        sample.taken_at,
        config.badge_columns,
        config.buckets_per_badge_column(),
        config,
    );
    push(
        client,
        config,
        &activity,
        sample.session.as_ref(),
        active,
        stopping,
    );
    Ok(())
}

/// One badge call to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadgeOp {
    Clear {
        workspace_id: String,
        token: String,
    },
    Set {
        workspace_id: String,
        token: &'static str,
        text: String,
    },
}

/// One workspace's rendered badge, which is all [`plan_for`] needs to know.
///
/// Split out from [`WorkspaceActivity`] so the ordering rules below can be
/// tested without a renderer, a store or a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBadge {
    pub workspace_id: String,
    pub tone: Tone,
    /// Exactly what `render::badge` produced. Empty means "draw nothing".
    pub text: String,
}

/// Turns "what is lit now" plus "what this cycle found" into the calls that close
/// the gap. Pure, so the ordering rules are testable without a socket:
///
/// * A tone flip clears the old token name *before* setting the new one. Tokens
///   are a merge patch, so an unmentioned name stays lit and herdr would render
///   two badges for one workspace.
/// * `render::badge` is the single author of badge text. An empty string is a
///   clear, never a draw: setting it would occupy the row with nothing.
/// * A workspace that dropped out of the report — closed — is cleared rather
///   than left to expire.
/// * The plan must be deterministic: sort anything that comes out of a `HashMap`
///   before emitting it, so tests and logs are reproducible.
/// * Only activity recorded by the live sample's session is a badge target.
///   Workspace ids are session-scoped, so pushing this session's badge to an id
///   retained from a previous session can land on a different workspace
///   entirely. A skipped row is still absent from the wanted set, so an active
///   token left by an earlier cycle is cleared rather than orphaned.
pub fn badge_plan(
    active: &HashMap<String, String>,
    activity: &[WorkspaceActivity],
    live_session: Option<&SessionMark>,
    config: &Config,
) -> Vec<BadgeOp> {
    let badges: Vec<WorkspaceBadge> = activity
        .iter()
        .filter(|activity| activity.is_session(live_session))
        .map(|activity| WorkspaceBadge {
            workspace_id: activity.workspace_id.clone(),
            tone: Tone::of(activity.state),
            text: crate::render::badge(activity, config),
        })
        .collect();
    plan_for(active, &badges)
}

/// The ordering rules of [`badge_plan`], over already-rendered badges.
pub fn plan_for(active: &HashMap<String, String>, badges: &[WorkspaceBadge]) -> Vec<BadgeOp> {
    let mut ops = Vec::new();
    let mut reported: Vec<&str> = Vec::new();
    let mut wanted: Vec<&str> = Vec::new();

    for badge in badges {
        reported.push(badge.workspace_id.as_str());
        let token = badge.tone.token_name();
        let previous = active.get(&badge.workspace_id).map(String::as_str);
        let next = if badge.text.is_empty() {
            None
        } else {
            Some(token)
        };

        if let Some(previous) = previous {
            if Some(previous) != next {
                ops.push(BadgeOp::Clear {
                    workspace_id: badge.workspace_id.clone(),
                    token: previous.to_string(),
                });
            }
        }
        if let Some(token) = next {
            wanted.push(badge.workspace_id.as_str());
            ops.push(BadgeOp::Set {
                workspace_id: badge.workspace_id.clone(),
                token,
                // Re-sent every cycle even when unchanged: the TTL is what makes
                // the badge self-heal, and it only refreshes on a write.
                text: badge.text.clone(),
            });
        }
    }

    let mut stale: Vec<(&String, &String)> = active
        .iter()
        .filter(|(workspace_id, _)| !wanted.contains(&workspace_id.as_str()))
        // Already cleared above by the tone-flip branch.
        .filter(|(workspace_id, _)| !reported.contains(&workspace_id.as_str()))
        .collect();
    // A HashMap iterates in an arbitrary order; sorting keeps the plan
    // reproducible for both tests and logs.
    stale.sort();
    for (workspace_id, token) in stale {
        ops.push(BadgeOp::Clear {
            workspace_id: workspace_id.clone(),
            token: token.clone(),
        });
    }

    ops
}

/// A plan collapsed to one `workspace.report_metadata` call per workspace.
///
/// Batching is verified against a live server, and it matters here for more than
/// round trips: a tone flip's clear-then-set lands as one merge patch, so there
/// is no window in which the old badge is gone and the new one has not arrived.
struct WorkspacePush {
    workspace_id: String,
    /// `None` clears the name, `Some` sets it.
    tokens: Vec<(String, Option<String>)>,
    /// The name this workspace ends up lit with, if the call succeeds.
    lit: Option<String>,
}

fn batch(plan: Vec<BadgeOp>) -> Vec<WorkspacePush> {
    let mut batched: Vec<WorkspacePush> = Vec::new();
    // First-seen order, which is the plan's order, which is the activity's
    // order: deterministic, and it keeps a workspace's clear ahead of its set
    // inside the one patch for anyone reading the wire.
    for op in plan {
        let (workspace_id, token, value) = match op {
            BadgeOp::Clear {
                workspace_id,
                token,
            } => (workspace_id, token, None),
            BadgeOp::Set {
                workspace_id,
                token,
                text,
            } => (workspace_id, token.to_string(), Some(text)),
        };
        let entry = match batched
            .iter_mut()
            .find(|entry| entry.workspace_id == workspace_id)
        {
            Some(entry) => entry,
            None => {
                batched.push(WorkspacePush {
                    workspace_id,
                    tokens: Vec::new(),
                    lit: None,
                });
                batched.last_mut().expect("just pushed")
            }
        };
        if value.is_some() {
            entry.lit = Some(token.clone());
        }
        entry.tokens.push((token, value));
    }
    batched
}

/// Executes a badge plan. Errors are reported per workspace and the cycle
/// continues: a swallowed push failure renders as a blank badge with nothing to
/// debug, and one bad workspace must not cost every other one its badge.
///
/// The `active` lock is held across **every** round trip, not merely around the
/// two touches of the map, and the `stopping` flag is re-read *after* acquiring
/// it. That pair is the shutdown interlock described in the module header: it is
/// what makes the signal thread's clears the last word on the wire instead of
/// merely the most recent thing it tried. Holding a lock across blocking I/O is
/// normally a smell; here the only other party wanting the lock is the thread
/// that is trying to shut this one down, and making it wait is precisely the
/// point.
fn push(
    client: &mut Herdr,
    config: &Config,
    activity: &[WorkspaceActivity],
    live_session: Option<&SessionMark>,
    active: &Mutex<HashMap<String, String>>,
    stopping: &AtomicBool,
) {
    let mut active = lock(active);
    if stopping.load(Ordering::SeqCst) {
        // `shutdown` got here first. Its clears have already gone out, and the
        // map it read is authoritative — re-lighting anything now would leave a
        // badge on a workspace nobody is watching any more.
        return;
    }

    let ttl_ms = config.ttl_ms();
    let plan = badge_plan(&active.clone(), activity, live_session, config);
    let mut lit: HashMap<String, String> = HashMap::new();

    for entry in batch(plan) {
        let tokens: Vec<(&str, Option<&str>)> = entry
            .tokens
            .iter()
            .map(|(token, value)| (token.as_str(), value.as_deref()))
            .collect();
        let names: Vec<&str> = tokens.iter().map(|(token, _)| *token).collect();
        if report_error(
            client.report_tokens(&entry.workspace_id, &tokens, ttl_ms),
            &entry.workspace_id,
            &names.join(","),
        ) {
            // Only a confirmed set is recorded as lit. A failed one is retried
            // next cycle rather than being remembered as done, and a failed
            // clear is forgotten rather than retried forever against a workspace
            // that may no longer exist — the TTL expires it within three cycles.
            if let Some(token) = entry.lit {
                lit.insert(entry.workspace_id, token);
            }
        }
    }

    // Still under the same guard: `shutdown` cannot observe a half-updated view
    // of what this push lit, so whatever it clears is exactly what is on screen.
    *active = lit;
}

/// Logs a failed push. Returns whether the call succeeded. A workspace that
/// closed under us is expected churn, not something to shout about.
fn report_error(result: Result<()>, workspace_id: &str, tokens: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(err) => {
            if herdr::error_code(&*err) != Some("workspace_not_found") {
                eprintln!("pulse: reporting {tokens} on {workspace_id} failed: {err}");
            }
            false
        }
    }
}

/// Clears every token this plugin owns on every current workspace.
///
/// Every name, not just the one we believe is lit: a daemon that was SIGKILLed
/// left whatever it had set, and a previous version of this plugin may have set
/// a name this one no longer uses. One report per workspace covers all three.
fn sweep(client: &mut Herdr) -> Result<()> {
    let sample = client.sample(crate::now_unix())?;
    let tokens: Vec<(&str, Option<&str>)> = Tone::ALL_TOKENS
        .iter()
        .map(|token| (*token, None))
        .collect();
    let names = Tone::ALL_TOKENS.join(",");

    let mut failures = 0usize;
    for workspace in &sample.workspaces {
        if !report_error(
            client.report_tokens(&workspace.workspace_id, &tokens, 0),
            &workspace.workspace_id,
            &names,
        ) {
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(format!("{failures} badge clears failed; see the messages above").into());
    }
    Ok(())
}

fn spawn_signal_thread(
    active: Arc<Mutex<HashMap<String, String>>>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            stopping.store(true, Ordering::SeqCst);
            // Who asked. `--disable` clears the enabled marker *before* it
            // signals — the lifecycle contract at the top of this module — so a
            // signal arriving with the marker gone is the user's own request,
            // and one arriving with it still set came from somewhere else: a
            // reboot, an OOM killer, somebody's `kill`. The two look identical
            // on the wire and mean different things to a reader deciding whether
            // to trust an empty sparkline.
            let reason = if is_enabled() {
                StopReason::Terminated
            } else {
                StopReason::Disabled
            };
            // Before the badge clears, not after: `shutdown` spends a round trip
            // per workspace and can be outlived by `--disable`'s own timeout, and
            // a stop nobody recorded reads as unknown.
            record_stop(reason, None);
            shutdown(&active);
            std::process::exit(0);
        }
    });
    Ok(())
}

/// Clears everything this daemon lit, over its **own** connection so it never
/// waits on the main loop's sleep or its in-flight round trip.
///
/// Taking the `active` lock here is the other half of the interlock in [`push`].
/// It blocks until any push already on the wire has finished and written down
/// what it lit, so the map read below is final rather than a snapshot of a race
/// still in progress. The wait is bounded by that push, worst case one
/// `herdr::IO_TIMEOUT` per workspace; if it outlasts `--disable`'s own
/// `STOP_TIMEOUT`, disable stops waiting and sweeps every workspace itself, so
/// the badges still come down — just from the other end.
fn shutdown(active: &Mutex<HashMap<String, String>>) {
    let mut active = lock(active);
    match Herdr::connect() {
        Ok(mut client) => {
            for (workspace_id, token) in active.iter() {
                report_error(client.clear_badge(workspace_id, token), workspace_id, token);
            }
        }
        // Not silent: without this line a killed daemon looks like it cleaned
        // up, and the badge lingers until its TTL expires.
        Err(err) => eprintln!("pulse: shutdown could not reach herdr: {err}"),
    }
    // A push that acquires the lock after this returns will bail on `stopping`,
    // but empty the map anyway: nothing downstream should be able to read a list
    // of tokens that have just been cleared as if they were still lit.
    active.clear();
    drop(active);

    clear_pid_file();
}

/// Sleeps in slices so a stop request is noticed without waiting out a whole
/// sampling interval.
fn nap(interval: Duration, stopping: &AtomicBool) {
    let deadline = Instant::now() + interval;
    while Instant::now() < deadline {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(LOOP_TICK.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panicking push must not take the badge state down with it; the data is a
    // plain map and stays consistent.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Process control
// ---------------------------------------------------------------------------

/// The arguments worth handing to the detached child, normalised to the
/// `--name value` spelling. Anything else on the command line is dropped.
pub fn forwarded_args(args: &[String]) -> Result<Vec<String>> {
    let mut forwarded = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(name) = FORWARDED.into_iter().find(|name| {
            arg == name
                || arg
                    .strip_prefix(*name)
                    .is_some_and(|tail| tail.starts_with('='))
        }) else {
            continue;
        };
        let value = match arg.split_once('=') {
            Some((_, value)) => value.to_string(),
            None => rest.next().ok_or(format!("{name} needs a value"))?.clone(),
        };
        forwarded.push(name.to_string());
        forwarded.push(value);
    }
    Ok(forwarded)
}

fn spawn_detached(forwarded: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("--daemon")
        .args(forwarded)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A daemon herdr spawned as a child dies with herdr. `setsid` puts it in its
    // own session so it survives; a double fork is not needed, and the extra
    // process would only make the pid we record harder to track.
    unsafe {
        command.pre_exec(|| {
            // EPERM here just means we are already a session leader.
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn()?;
    write_pid(child.id());
    Ok(())
}

fn request_stop(pid: i32) {
    // SIGTERM, not SIGKILL: the daemon's handler is what clears the badges.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

fn await_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(STOP_POLL);
    }
    !is_alive(pid)
}

fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks for existence without delivering anything. EPERM means the
    // process exists but belongs to someone else.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Guards against pid reuse. The state dir outlives reboots, so a recorded pid
/// can easily belong to something else entirely by the time we read it.
#[cfg(target_os = "linux")]
fn same_program(pid: i32) -> bool {
    let ours = fs::read_to_string("/proc/self/comm");
    let theirs = fs::read_to_string(format!("/proc/{pid}/comm"));
    match (ours, theirs) {
        (Ok(ours), Ok(theirs)) => ours.trim() == theirs.trim(),
        // /proc unreadable (hidepid, a stripped container): fall back to trusting
        // the liveness probe rather than killing a live daemon's marker.
        _ => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn same_program(_pid: i32) -> bool {
    // No portable equivalent of /proc/<pid>/comm; liveness is all we have.
    true
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

/// The pid of a daemon that is live *right now*, or `None`. A stale or reused pid
/// file is swept as a side effect so the next verb starts from a clean state.
///
/// Must guard against **pid reuse**: the state dir outlives reboots, so compare
/// `/proc/<pid>/comm` against our own on Linux and degrade to a bare liveness
/// probe elsewhere.
pub fn live_pid() -> Option<i32> {
    let Some(recorded) = read_pid() else {
        // An unparseable marker names no process at all, so it is swept for the
        // same reason a stale one is: the next verb has to start from a clean
        // state. Leaving it would park a file in the state dir that no verb ever
        // removes and that reads, to anyone inspecting the directory, as a
        // daemon that is still running.
        clear_pid_file();
        return None;
    };
    if is_alive(recorded) && same_program(recorded) {
        return Some(recorded);
    }
    clear_pid_file();
    None
}

pub fn read_pid() -> Option<i32> {
    fs::read_to_string(config::pid_file())
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
}

pub fn write_pid(pid: u32) {
    // Best effort: an unwritable state dir must not fail the user's action, but
    // it must not be silent either — without the marker, `--enable` will happily
    // start a second daemon.
    let path = config::pid_file();
    if let Err(err) = write_marker(&path, &pid.to_string()) {
        eprintln!("pulse: could not record pid in {}: {err}", path.display());
    }
}

/// Removes the pid file, but only if it still names this process or a dead one,
/// so a successor daemon's marker is never deleted.
pub fn clear_pid_file() {
    match read_pid() {
        Some(pid) if pid != std::process::id() as i32 && is_alive(pid) && same_program(pid) => {}
        _ => {
            let _ = fs::remove_file(config::pid_file());
        }
    }
}

/// Did the user ever ask for a daemon? Consulted by `--restore`.
pub fn is_enabled() -> bool {
    config::enabled_flag().exists()
}

pub fn mark_enabled(enabled: bool) {
    let path = config::enabled_flag();
    let outcome = if enabled {
        write_marker(&path, "1")
    } else {
        match fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(err) = outcome {
        eprintln!("pulse: could not update {}: {err}", path.display());
    }
}

fn write_marker(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}
