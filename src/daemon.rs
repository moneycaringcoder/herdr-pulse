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
//! | `--enable`  | mark enabled **first**, no-op if OWNER is held, else transfer OWNER to one detached child |
//! | `--disable` | mark disabled **first**, request stop, **await OWNER release**, then sweep this socket's workspaces |
//! | `--toggle`  | disable if OWNER is held, otherwise enable |
//! | `--restore` | silent no-op unless the enabled marker is set and OWNER is free |
//!
//! Awaiting OWNER release on `--disable` is load-bearing: the stop request only
//! *posts*, while the daemon can still be clearing badges and holding in-memory
//! history. The PID is only the signal target; the lifetime-held flock proves
//! when that writer is gone. A per-namespace control flock serializes lifecycle
//! transitions so enable cannot pass disable in the stopping window. Bound the
//! wait (~3 s) so disable can never hang.
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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, SessionPaths};
use crate::herdr::{self, Herdr};
use crate::history;
use crate::model::{SessionMark, Tone, WorkspaceActivity};
use crate::supervise;
use crate::Result;

/// The stop request only posts a signal; the daemon still has to clear its
/// badges. Bounded so `--disable` can never hang on a wedged daemon.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(25);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const OWNER_FD_ENV: &str = "PULSE_OWNER_FD";
const READY_FD_ENV: &str = "PULSE_READY_FD";

/// Serializes every lifecycle transition for one socket namespace.
pub(crate) struct ControlGuard {
    _file: File,
}

/// Authoritative sampler ownership, retained for the full daemon lifetime.
pub(crate) struct OwnerGuard(File);

impl ControlGuard {
    pub(crate) fn acquire(paths: &SessionPaths) -> Result<Self> {
        let file = open_lock(&paths.control_lock())?;
        flock_exclusive(&file, false)?;
        Ok(Self { _file: file })
    }
}

impl OwnerGuard {
    fn try_acquire(paths: &SessionPaths) -> Result<Option<Self>> {
        let file = open_lock(&paths.owner_lock())?;
        match flock_exclusive(&file, true) {
            Ok(()) => Ok(Some(Self(file))),
            Err(err) if is_lock_busy(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn from_inherited(paths: &SessionPaths, fd: RawFd) -> Result<Self> {
        if fd <= libc::STDERR_FILENO {
            return Err("invalid inherited sampler owner descriptor".into());
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let actual = unsafe { stat.assume_init() };
        let expected = fs::metadata(paths.owner_lock())?;
        if actual.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat_device(&actual) != expected.dev()
            || actual.st_ino != expected.ino()
        {
            return Err(format!(
                "inherited sampler owner descriptor does not name {}",
                paths.owner_lock().display()
            )
            .into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        // Re-taking the same OFD lock succeeds. A separately opened descriptor
        // for the right inode fails while the real owner is live.
        flock_exclusive(&file, true)?;
        Ok(Self(file))
    }
}

#[cfg(target_os = "macos")]
fn stat_device(stat: &libc::stat) -> u64 {
    stat.st_dev as u64
}

#[cfg(not(target_os = "macos"))]
fn stat_device(stat: &libc::stat) -> u64 {
    stat.st_dev
}

fn open_lock(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn flock_exclusive(file: &File, nonblocking: bool) -> io::Result<()> {
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn is_lock_busy(err: &io::Error) -> bool {
    err.raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
}

/// The main loop wakes at least this often so a stop request is noticed promptly
/// even with a long sampling interval.
const LOOP_TICK: Duration = Duration::from_millis(250);

/// Valued options the detached child is given a copy of. It re-reads the config
/// file but never sees the user's command line.
pub const FORWARDED: [&str; 4] = [
    "--interval",
    "--bucket-seconds",
    "--retention-buckets",
    "--columns",
];

/// Bare switches the child needs too.
///
/// Separate from [`FORWARDED`] because they take no value, and folded in for the
/// same reason the valued ones are: `--agents` changes what the sampler
/// *records*, so a `pulse --enable --agents` that did not reach the child would
/// quietly record nothing and leave the user reading empty agent rows.
pub const FORWARDED_SWITCHES: [&str; 1] = ["--agents"];

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
fn record_stop(paths: &SessionPaths, reason: StopReason, detail: Option<&str>) {
    let at = crate::now_unix();
    let line = match detail {
        Some(detail) => format!("{}\n{at}\n{}\n", reason.as_str(), one_line(detail)),
        None => format!("{}\n{at}\n", reason.as_str()),
    };
    let _ = fs::write(paths.stop_marker(), line);
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
fn clear_stop(paths: &SessionPaths) {
    let _ = fs::remove_file(paths.stop_marker());
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
pub fn stop_report(paths: &SessionPaths) -> Result<Option<SamplerStop>> {
    if is_running(paths) {
        return Ok(None);
    }
    if let Some(stop) = read_stop_marker(paths) {
        return Ok(Some(stop));
    }
    if is_enabled(paths) {
        return Ok(Some(SamplerStop {
            reason: StopReason::Unknown,
            at: None,
            detail: None,
        }));
    }
    Ok(None)
}
fn read_stop_marker(paths: &SessionPaths) -> Option<SamplerStop> {
    let raw = fs::read_to_string(paths.stop_marker()).ok()?;
    let mut lines = raw.lines();
    let reason = StopReason::parse(lines.next()?.trim());
    Some(SamplerStop {
        reason,
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
fn install_panic_reporter(paths: SessionPaths) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_stop(&paths, StopReason::Failed, Some(&panic_detail(info)));
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
pub fn record_failure(paths: &SessionPaths, detail: &str) {
    record_stop(paths, StopReason::Failed, Some(detail));
}
pub fn enable(paths: &SessionPaths, args: &[String]) -> Result<()> {
    let forwarded = forwarded_args(args)?;
    crate::config::load_with_args(args)?;
    let control = ControlGuard::acquire(paths)?;
    mark_enabled(paths, true)?;
    if owner_is_held(paths)? {
        return Ok(());
    }
    remove_stale_pid(paths)?;
    if supervise::is_installed(paths) {
        supervise::start(paths)?;
        drop(control);
        return await_started(paths, START_TIMEOUT);
    }
    spawn_detached(paths, &forwarded)
}

pub fn disable(paths: &SessionPaths) -> Result<()> {
    let _control = ControlGuard::acquire(paths)?;
    disable_locked(paths)
}

fn disable_locked(paths: &SessionPaths) -> Result<()> {
    mark_enabled(paths, false)?;
    if supervise::is_installed(paths) {
        if let Err(err) = supervise::stop(paths) {
            eprintln!("pulse: {err}");
        }
    }
    stop_sampler_locked(paths, false)?;
    let mut client = Herdr::connect_at(&paths.socket_path)?;
    sweep(&mut client)
}

/// Deletes only this socket namespace's history while excluding its writer.
pub fn forget_history(paths: &SessionPaths) -> Result<()> {
    let control = ControlGuard::acquire(paths)?;
    let was_live = owner_is_held(paths)?;
    if !was_live {
        remove_stale_pid(paths)?;
        history::forget(paths)?;
        println!("pulse: recorded history forgotten; the sampler remains stopped.");
        return Ok(());
    }

    let supervised = supervise::is_installed(paths);
    if supervised {
        supervise::stop(paths).map_err(|err| {
            format!("cannot forget history: could not stop the sampler supervisor: {err}")
        })?;
    }
    if let Err(stop_error) = stop_sampler_locked(paths, true) {
        if supervised {
            return match supervise::restore_start_at_login(paths) {
                Ok(()) => Err(format!(
                    "cannot forget history: the running sampler could not be stopped \
                     ({stop_error}); start-at-login supervision was restored without starting \
                     a second sampler"
                )
                .into()),
                Err(restore_error) => Err(format!(
                    "cannot forget history: the running sampler could not be stopped \
                     ({stop_error}), and start-at-login supervision could not be restored: \
                     {restore_error}"
                )
                .into()),
            };
        }
        return Err(format!(
            "cannot forget history: the running sampler could not be stopped: {stop_error}"
        )
        .into());
    }

    if let Err(delete_error) = history::forget(paths) {
        let restored = if supervised {
            let started = supervise::start(paths);
            drop(control);
            started.and_then(|()| await_started(paths, START_TIMEOUT))
        } else {
            spawn_detached(paths, &[])
        };
        return match restored {
            Ok(()) => Err(format!(
                "could not forget recorded history ({delete_error}); the sampler was restored"
            )
            .into()),
            Err(restore_error) => Err(format!(
                "could not forget recorded history ({delete_error}), and could not restore the \
                 sampler: {restore_error}"
            )
            .into()),
        };
    }
    if supervised {
        supervise::start(paths)?;
        drop(control);
        await_started(paths, START_TIMEOUT)?;
    } else {
        spawn_detached(paths, &[])?;
    }
    let owner = if supervised {
        "its supervisor"
    } else {
        "a detached process"
    };
    println!("pulse: recorded history forgotten; the sampler restarted under {owner}.");
    Ok(())
}

/// Stops the current owner. Caller must hold CONTROL.
pub(crate) fn stop_sampler_locked(paths: &SessionPaths, strict: bool) -> Result<()> {
    if !owner_is_held(paths)? {
        remove_stale_pid(paths)?;
        return Ok(());
    }
    let pid = read_pid(paths).ok_or_else(|| {
        format!(
            "sampler ownership is held but {} has no readable pid",
            paths.pid_file().display()
        )
    })?;
    request_stop(pid);
    if !await_owner_release(paths, STOP_TIMEOUT)? {
        let message = format!("sampler {pid} did not release ownership within {STOP_TIMEOUT:?}");
        if strict {
            return Err(message.into());
        }
        eprintln!("pulse: {message}");
        return Ok(());
    }
    remove_pid_if_equals(paths, pid);
    Ok(())
}

pub fn toggle(paths: &SessionPaths, args: &[String]) -> Result<()> {
    let forwarded = forwarded_args(args)?;
    crate::config::load_with_args(args)?;
    let control = ControlGuard::acquire(paths)?;
    if owner_is_held(paths)? {
        return disable_locked(paths);
    }
    mark_enabled(paths, true)?;
    remove_stale_pid(paths)?;
    if supervise::is_installed(paths) {
        supervise::start(paths)?;
        drop(control);
        await_started(paths, START_TIMEOUT)
    } else {
        spawn_detached(paths, &forwarded)
    }
}

/// Herdr startup hook. Rechecks wanted state and ownership under CONTROL.
pub fn restore(paths: &SessionPaths) -> Result<()> {
    let _control = ControlGuard::acquire(paths)?;
    if !is_enabled(paths) || owner_is_held(paths)? || supervise::is_installed(paths) {
        return Ok(());
    }
    remove_stale_pid(paths)?;
    spawn_detached(paths, &[])
}

/// Foreground daemon entry. A detached child receives an already-owned flock;
/// direct and supervisor launches claim it themselves under CONTROL.
pub fn run_daemon(paths: &SessionPaths, config: &Config) -> Result<()> {
    let inherited_owner = inherited_fd(OWNER_FD_ENV)?;
    let inherited_ready = inherited_fd(READY_FD_ENV)?;
    let (inherited, ready_fd) =
        match (inherited_owner, inherited_ready) {
            (Some(owner), Some(ready)) => (Some(owner), Some(ready)),
            (None, None) => (None, None),
            _ => return Err(
                "inherited sampler ownership and readiness descriptors must be provided together"
                    .into(),
            ),
        };
    if let (Some(owner), Some(ready)) = (inherited, ready_fd) {
        if owner == ready {
            return Err("sampler owner and readiness descriptors must be distinct".into());
        }
        validate_ready_fd(ready)?;
        set_cloexec(owner, true)?;
        set_cloexec(ready, true)?;
    }
    let owner = if let Some(fd) = inherited {
        OwnerGuard::from_inherited(paths, fd)?
    } else {
        let control = ControlGuard::acquire(paths)?;
        if !is_enabled(paths) {
            signal_ready(ready_fd, b'E');
            return Ok(());
        }
        let Some(owner) = OwnerGuard::try_acquire(paths)? else {
            signal_ready(ready_fd, b'E');
            return Ok(());
        };
        remove_stale_pid(paths)?;
        publish_pid(paths, std::process::id())?;
        clear_stop(paths);
        signal_ready(ready_fd, b'R');
        drop(control);
        owner
    };

    if inherited.is_some() {
        if let Err(err) = publish_pid(paths, std::process::id()) {
            signal_ready(ready_fd, b'E');
            return Err(err);
        }
        clear_stop(paths);
        signal_ready(ready_fd, b'R');
    }

    install_panic_reporter(paths.clone());
    let outcome = run(paths, config, owner);
    if let Err(err) = &outcome {
        record_stop(paths, StopReason::Failed, Some(&err.to_string()));
    }
    outcome
}

fn run(paths: &SessionPaths, config: &Config, _owner: OwnerGuard) -> Result<()> {
    let active: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    spawn_signal_thread(paths.clone(), Arc::clone(&active), Arc::clone(&stopping))?;

    let mut history = history::load(paths, config);
    let mut client: Option<Herdr> = None;
    let mut reported_save_failure = false;

    loop {
        if stopping.load(Ordering::SeqCst) {
            loop {
                std::thread::park();
            }
        }

        if client.is_none() {
            match Herdr::connect_at(&paths.socket_path) {
                Ok(connected) => client = Some(connected),
                Err(err) => eprintln!("pulse: cannot reach herdr: {err}"),
            }
        }
        if let Some(connected) = client.as_mut() {
            if let Err(err) = cycle(
                paths,
                connected,
                config,
                &mut history,
                &active,
                &stopping,
                &mut reported_save_failure,
            ) {
                eprintln!("pulse: sample failed: {err}");
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
    paths: &SessionPaths,
    client: &mut Herdr,
    config: &Config,
    history: &mut history::History,
    active: &Mutex<HashMap<String, String>>,
    stopping: &AtomicBool,
    reported_save_failure: &mut bool,
) -> Result<()> {
    let sample = client.sample(crate::now_unix())?;
    history.record(&sample, config);

    match history::save(paths, history, config) {
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
    paths: SessionPaths,
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
            let reason = if is_enabled(&paths) {
                StopReason::Terminated
            } else {
                StopReason::Disabled
            };
            record_stop(&paths, reason, None);
            shutdown(&paths, &active);
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
fn shutdown(paths: &SessionPaths, active: &Mutex<HashMap<String, String>>) {
    let mut active = lock(active);
    match Herdr::connect_at(&paths.socket_path) {
        Ok(mut client) => {
            for (workspace_id, token) in active.iter() {
                report_error(client.clear_badge(workspace_id, token), workspace_id, token);
            }
        }
        Err(err) => eprintln!("pulse: shutdown could not reach herdr: {err}"),
    }
    active.clear();
    drop(active);
    remove_pid_if_equals(paths, std::process::id() as i32);
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
        if let Some(switch) = FORWARDED_SWITCHES.into_iter().find(|switch| arg == switch) {
            // Once, however many times the user typed it: a repeated switch is
            // the same instruction, and a child argv that grows with it is a
            // child argv nobody can read in `ps`.
            if !forwarded.iter().any(|held| held == switch) {
                forwarded.push(switch.to_string());
            }
            continue;
        }
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

fn spawn_detached(paths: &SessionPaths, forwarded: &[String]) -> Result<()> {
    let Some(owner) = OwnerGuard::try_acquire(paths)? else {
        return Ok(());
    };
    remove_stale_pid(paths)?;

    let (mut ready_read, ready_write) = pipe_cloexec()?;
    let owner_fd = owner.0.as_raw_fd();
    let ready_fd = ready_write.as_raw_fd();
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .env(
            crate::config::SOCKET_IS_DEFAULT_ENV,
            if paths.scope_key.is_none() { "1" } else { "0" },
        )
        .arg("--daemon")
        .args(forwarded)
        .env("HERDR_SOCKET_PATH", &paths.socket_path)
        .env("HERDR_PLUGIN_STATE_DIR", &paths.state_root)
        .env(OWNER_FD_ENV, owner_fd.to_string())
        .env(READY_FD_ENV, ready_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            set_cloexec(owner_fd, false)?;
            set_cloexec(ready_fd, false)?;
            libc::setsid();
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    drop(ready_write);
    match await_child_ready(&mut ready_read, START_TIMEOUT) {
        Ok(()) => {
            // The child's duplicate refers to the same locked open-file
            // description, so this close cannot release its ownership.
            drop(owner);
            Ok(())
        }
        Err(err) => {
            let child_pid = child.id() as i32;
            terminate_child(&mut child);
            drop(owner);
            remove_pid_if_equals(paths, child_pid);
            Err(err)
        }
    }
}

fn pipe_cloexec() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { File::from_raw_fd(fds[0]) };
    let write = unsafe { File::from_raw_fd(fds[1]) };
    set_cloexec(read.as_raw_fd(), true).and_then(|()| set_cloexec(write.as_raw_fd(), true))?;
    Ok((read, write))
}

fn set_cloexec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn await_child_ready(read: &mut File, timeout: Duration) -> Result<()> {
    let mut pollfd = libc::pollfd {
        fd: read.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result > 0 {
            let mut byte = [0_u8; 1];
            return match read.read(&mut byte) {
                Ok(1) if byte[0] == b'R' => Ok(()),
                Ok(1) if byte[0] == b'E' => Err("sampler failed before publishing its pid".into()),
                Ok(_) => {
                    Err("sampler closed its readiness channel before publishing its pid".into())
                }
                Err(err) => Err(err.into()),
            };
        }
        if result == 0 {
            return Err(format!("sampler did not become ready within {timeout:?}").into());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err.into());
        }
    }
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn inherited_fd(name: &str) -> Result<Option<RawFd>> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} is not valid UTF-8"))?;
    let fd = value
        .parse::<RawFd>()
        .map_err(|_| format!("{name} is not a valid descriptor"))?;
    Ok(Some(fd))
}

fn validate_ready_fd(fd: RawFd) -> Result<()> {
    if fd <= libc::STDERR_FILENO {
        return Err("invalid inherited sampler readiness descriptor".into());
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err("inherited sampler readiness descriptor is not a pipe".into());
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Err("inherited sampler readiness descriptor is not writable".into());
    }
    Ok(())
}

fn signal_ready(fd: Option<RawFd>, byte: u8) {
    let Some(fd) = fd else {
        return;
    };
    let mut file = unsafe { File::from_raw_fd(fd) };
    let _ = file.write_all(&[byte]);
}

fn request_stop(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

fn pid_is_live(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists && same_program(pid)
}

#[cfg(target_os = "linux")]
fn same_program(pid: i32) -> bool {
    let ours = fs::read_to_string("/proc/self/comm");
    let theirs = fs::read_to_string(format!("/proc/{pid}/comm"));
    match (ours, theirs) {
        (Ok(ours), Ok(theirs)) => ours.trim() == theirs.trim(),
        _ => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn same_program(_pid: i32) -> bool {
    true
}

fn owner_is_held(paths: &SessionPaths) -> Result<bool> {
    Ok(OwnerGuard::try_acquire(paths)?.is_none())
}

fn await_owner_release(paths: &SessionPaths, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(owner) = OwnerGuard::try_acquire(paths)? {
            drop(owner);
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(STOP_POLL);
    }
}

pub(crate) fn await_started(paths: &SessionPaths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if is_running(paths) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "sampler did not acquire ownership and publish {} within {timeout:?}",
                paths.pid_file().display()
            )
            .into());
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// Published PID for user-facing status and signaling. OWNER remains the
/// lifecycle authority; PID observation is deliberately non-mutating so a
/// status read can never steal a starting daemon's flock.
pub fn live_pid(paths: &SessionPaths) -> Result<Option<i32>> {
    Ok(read_pid(paths).filter(|pid| pid_is_live(*pid)))
}

pub fn is_running(paths: &SessionPaths) -> bool {
    read_pid(paths).is_some_and(pid_is_live)
}

pub fn read_pid(paths: &SessionPaths) -> Option<i32> {
    fs::read_to_string(paths.pid_file())
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
}

fn publish_pid(paths: &SessionPaths, pid: u32) -> Result<()> {
    let target = paths.pid_file();
    let temp = paths.state_dir.join("sampler.pid.tmp");
    write_marker(&temp, &pid.to_string())?;
    fs::rename(&temp, &target).map_err(|err| {
        format!(
            "cannot publish sampler pid from {} to {}: {err}",
            temp.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn remove_stale_pid(paths: &SessionPaths) -> Result<()> {
    match fs::remove_file(paths.pid_file()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn remove_pid_if_equals(paths: &SessionPaths, expected: i32) {
    if read_pid(paths) == Some(expected) {
        let _ = fs::remove_file(paths.pid_file());
    }
}

pub fn is_enabled(paths: &SessionPaths) -> bool {
    paths.enabled_flag().exists()
}

pub fn mark_enabled(paths: &SessionPaths, enabled: bool) -> Result<()> {
    let path = paths.enabled_flag();
    if enabled {
        write_marker(&path, "1")?;
    } else {
        match fs::remove_file(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
            Ok(()) => {}
        }
    }
    Ok(())
}

fn write_marker(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}
