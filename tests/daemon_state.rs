//! Marker-file, lifecycle and badge-plan tests for the sampler daemon.
//!
//! These run against a temp state dir, never the user's real one. Where a test
//! needs "a live daemon" it uses either this process's own pid or a deliberately
//! orphaned helper process — never a real sampler, and never a `--disable`
//! aimed at ourselves, which would deliver SIGTERM to the test runner.
//!
//! The badge plan is covered at two levels. `plan_for` takes already-rendered
//! badges, so the clear-before-set ordering rules can be enumerated without a
//! store, a renderer or a socket. `badge_plan` is then exercised over activity
//! built by driving the **real** store at the **real** geometry `daemon::cycle`
//! asks for, because the seam where store geometry meets renderer geometry is
//! the one thing the pure tests cannot see — and a hand-built series is free to
//! have a width the store could never emit.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use pulse::config::{self, Config, MAX_INTERVAL_SECONDS, MIN_INTERVAL_SECONDS};
use pulse::daemon::{self, BadgeOp, WorkspaceBadge};
use pulse::history::History;
use pulse::model::WorkspaceObservation;
use pulse::model::{AgentObservation, AgentState, Sample, SessionMark, Tone, WorkspaceActivity};
use pulse::supervise;

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

fn badge(workspace_id: &str, tone: Tone, text: &str) -> WorkspaceBadge {
    WorkspaceBadge {
        workspace_id: workspace_id.to_string(),
        tone,
        text: text.to_string(),
    }
}

fn lit(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(id, token)| (id.to_string(), token.to_string()))
        .collect()
}

/// The state and config dirs come from process-global env vars, so these tests
/// have to run one at a time.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_temp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "pulse-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ))
}

struct TempDirs {
    root: PathBuf,
}

impl TempDirs {
    fn new(tag: &str) -> Self {
        let root = unique_temp(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).expect("state dir");
        std::fs::create_dir_all(root.join("config")).expect("config dir");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", root.join("state"));
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", root.join("config"));
        // Nothing here may reach a real herdr, and a `--disable` that found one
        // would clear the user's live badges.
        std::env::set_var("HERDR_SOCKET_PATH", root.join("absent.sock"));
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
}

impl Drop for TempDirs {
    fn drop(&mut self) {
        // Best effort: a test that made the state dir unwritable has to hand the
        // permissions back before the tree can be removed.
        let _ = std::fs::set_permissions(
            self.root.join("state"),
            std::fs::Permissions::from_mode(0o755),
        );
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
        std::env::remove_var("HERDR_SOCKET_PATH");
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_pid_file(contents: &str) {
    std::fs::write(config::pid_file(), contents).expect("write pid file");
}

fn exists(path: &Path) -> bool {
    path.exists()
}

/// A pid that is guaranteed dead: a child we have already reaped. Immediate
/// reuse by another process is vanishingly unlikely within one test.
fn reaped_pid() -> u32 {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

fn is_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// This process's `comm`, which is what `/proc/<pid>/comm` comparison keys on.
#[cfg(target_os = "linux")]
fn our_comm() -> String {
    std::fs::read_to_string("/proc/self/comm")
        .expect("/proc/self/comm")
        .trim()
        .to_string()
}

/// Elsewhere there is no `comm` to match, so the name is arbitrary — the pid
/// guard degrades to a bare liveness probe on those platforms.
#[cfg(not(target_os = "linux"))]
fn our_comm() -> String {
    "pulse-sleeper".to_string()
}

/// A live process that is **not** this one but presents the same `comm`, which
/// is exactly what a successor daemon looks like to the pid guard.
///
/// Deliberately orphaned: `sh` starts it in the background and exits, so init
/// reaps it. A child of the test process would instead become a zombie when
/// killed, and a zombie still answers `kill(pid, 0)` — the liveness probe would
/// keep saying yes and `--disable` would wait out its whole timeout.
struct Sleeper {
    pid: i32,
    dir: PathBuf,
}

impl Sleeper {
    fn spawn() -> Option<Self> {
        let source = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists())?;
        let dir = unique_temp("sleeper");
        std::fs::create_dir_all(&dir).ok()?;
        // The copy's file name becomes its `comm`, so it matches ours.
        let path = dir.join(our_comm());
        std::fs::copy(&source, &path).ok()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "{} 300 </dev/null >/dev/null 2>&1 & echo $!",
                path.display()
            ))
            .output()
            .ok()?;
        let pid: i32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;

        // `sh` reports the pid before the exec completes, so wait for the new
        // program to actually be in place before anyone reads its `comm`.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            #[cfg(target_os = "linux")]
            let ready = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|comm| comm.trim() == our_comm())
                .unwrap_or(false);
            #[cfg(not(target_os = "linux"))]
            let ready = is_alive(pid);
            if ready {
                return Some(Self { pid, dir });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = std::fs::remove_dir_all(&dir);
        None
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Waits for a pid to leave the process table.
fn await_death(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    !is_alive(pid)
}

/// Reaps whatever `--enable` / `--restore` detached, so a spawned child can
/// never outlive the test that created it.
///
/// The `waitpid` is load-bearing *here* and nowhere else: the detached daemon is
/// a direct child of whoever spawned it, and in production that parent exits
/// immediately so init reaps the orphan. A test runner does not exit, so without
/// this the dead child stays a zombie — and a zombie still answers
/// `kill(pid, 0)`, which would make `clear_pid_file` treat it as a live
/// successor and refuse to remove the marker.
fn reap_spawned() {
    if let Some(pid) = daemon::read_pid() {
        if pid != std::process::id() as i32 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            await_death(pid, Duration::from_secs(2));
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
        }
    }
    daemon::clear_pid_file();
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[test]
fn enabled_flag_round_trips() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("enabled");

    assert!(
        !daemon::is_enabled(),
        "a fresh state dir means never enabled"
    );

    daemon::mark_enabled(true);
    assert!(daemon::is_enabled());
    assert!(exists(&config::enabled_flag()));

    daemon::mark_enabled(false);
    assert!(!daemon::is_enabled());
    assert!(!exists(&config::enabled_flag()));

    // Disabling twice is a no-op, not an error: the marker is already gone.
    daemon::mark_enabled(false);
    assert!(!daemon::is_enabled());

    // And enabling twice leaves exactly one marker.
    daemon::mark_enabled(true);
    daemon::mark_enabled(true);
    assert!(daemon::is_enabled());
}

// ---------------------------------------------------------------------------
// Why the sampler stopped
// ---------------------------------------------------------------------------

fn write_stop_marker(contents: &str) {
    std::fs::write(config::stop_marker(), contents).expect("write stop marker");
}

#[test]
fn a_live_sampler_has_nothing_to_explain() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-live");
    // Our own pid stands in for a live daemon.
    daemon::write_pid(std::process::id());
    // Even with a marker left over from an earlier run: a gap while a sampler is
    // running means herdr was unreachable, not that the sampler stopped.
    write_stop_marker("disabled\n1700000000\n");

    assert!(daemon::stop_report().is_none());

    daemon::clear_pid_file();
}

#[test]
fn a_recorded_stop_is_reported_with_its_reason_and_time() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-recorded");

    for (written, expected) in [
        ("disabled", daemon::StopReason::Disabled),
        ("terminated", daemon::StopReason::Terminated),
        ("failed", daemon::StopReason::Failed),
    ] {
        write_stop_marker(&format!("{written}\n1700000000\n"));
        let stop = daemon::stop_report().expect("a stopped sampler");
        assert_eq!(stop.reason, expected, "marker {written:?}");
        assert_eq!(stop.at, Some(1_700_000_000));
        assert_eq!(stop.detail, None);
    }
}

#[test]
fn a_failure_carries_its_one_line_of_detail() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-detail");
    write_stop_marker("failed\n1700000000\ncannot reach herdr at /nowhere.sock\n");

    let stop = daemon::stop_report().expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Failed);
    assert_eq!(
        stop.detail.as_deref(),
        Some("cannot reach herdr at /nowhere.sock")
    );
}

#[test]
fn a_run_that_left_no_marker_reads_as_unknown_and_never_as_disabled() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-killed");
    // What a SIGKILL leaves: the user still wants a sampler, there is not one,
    // and nothing was written on the way out.
    daemon::mark_enabled(true);

    let stop = daemon::stop_report().expect("a sampler that is wanted and absent");
    assert_eq!(
        stop.reason,
        daemon::StopReason::Unknown,
        "a run that said nothing must not be reported as a tidy shutdown"
    );
    assert_eq!(stop.at, None, "nobody was there to record a time");
}

#[test]
fn a_marker_nobody_can_read_is_unknown_rather_than_ignored() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-garbage");
    daemon::mark_enabled(true);

    // A reason from a newer pulse, or a hand-edited file. Either way it is a word
    // this build cannot interpret, and interpreting it anyway is how a guess gets
    // in.
    write_stop_marker("evaporated\n1700000000\n");
    let stop = daemon::stop_report().expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Unknown);
    assert_eq!(
        stop.at,
        Some(1_700_000_000),
        "the marker was parsed, not skipped: without this the enabled-flag \
         fallback would produce the same reason and the test would prove nothing"
    );

    // A marker with no timestamp still names its reason; only the "when"
    // degrades.
    write_stop_marker("terminated\n");
    let stop = daemon::stop_report().expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Terminated);
    assert_eq!(stop.at, None);
}

#[test]
fn a_sampler_that_was_never_enabled_here_has_nothing_to_report() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-fresh");

    // A fresh state dir is not a stopped sampler. Reporting one would put a
    // reason under every empty pane on a machine that has never run `--enable`.
    assert!(daemon::stop_report().is_none());
}

#[test]
fn a_real_panic_string_keeps_its_message_in_the_marker() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-panic");
    daemon::mark_enabled(true);

    // What `std` hands a panic hook, verbatim in shape: the location first, the
    // message on the *next* line. A fold that cut at the first newline would
    // keep the file and line and throw the sentence away — in the one place a
    // detached daemon can still say what happened.
    daemon::record_failure("panicked at src/daemon.rs:412:9:\nthe ring length was zero");

    let stop = daemon::stop_report().expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Failed);
    let detail = stop.detail.expect("a failure carries its detail");
    assert!(
        detail.contains("the ring length was zero"),
        "the message is the part a reader needs: {detail:?}"
    );
    assert!(
        !detail.contains('\n'),
        "the marker is line-delimited: {detail:?}"
    );
}

#[test]
fn an_enormous_detail_is_trimmed_rather_than_written_whole() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stop-huge");
    daemon::mark_enabled(true);

    daemon::record_failure(&"x".repeat(10_000));

    let stop = daemon::stop_report().expect("a stopped sampler");
    let detail = stop.detail.expect("detail");
    assert!(detail.len() <= 200, "{} chars", detail.len());
}

#[test]
fn the_markers_are_created_even_when_the_state_dir_does_not_exist_yet() {
    let _guard = env_lock();
    let dirs = TempDirs::new("mkdir");
    // herdr creates the state dir for plugins it spawns, but a hand-run
    // `--enable` may get there first.
    let nested = dirs.state().join("deep").join("deeper");
    std::env::set_var("HERDR_PLUGIN_STATE_DIR", &nested);

    daemon::mark_enabled(true);
    daemon::write_pid(std::process::id());

    assert!(exists(&nested.join("enabled")));
    assert_eq!(daemon::read_pid(), Some(std::process::id() as i32));
    daemon::clear_pid_file();
}

#[test]
fn no_pid_file_means_no_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("nopid");

    assert_eq!(daemon::live_pid(), None);
    assert_eq!(daemon::read_pid(), None);
    // Clearing a marker that is not there is not an error.
    daemon::clear_pid_file();
}

#[test]
fn a_stale_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stale");

    write_pid_file(&reaped_pid().to_string());

    assert_eq!(daemon::live_pid(), None, "the recorded process is gone");
    assert!(
        !exists(&config::pid_file()),
        "a stale marker is swept, so the next --enable can spawn"
    );
}

#[test]
fn a_malformed_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("garbage");

    for contents in [
        "",
        "   ",
        "\n",
        "not-a-pid",
        "0",
        "-1",
        "12.5",
        "99999999999999999999",
        "1 2 3",
        "\u{0}",
    ] {
        write_pid_file(contents);
        assert_eq!(daemon::read_pid(), None, "pid file contents {contents:?}");
        assert_eq!(daemon::live_pid(), None, "pid file contents {contents:?}");
        assert!(
            !exists(&config::pid_file()),
            "an unreadable marker is swept too: {contents:?}"
        );
    }

    // Surrounding whitespace is fine — the file is written with `to_string`, but
    // a user may have echoed into it.
    write_pid_file(&format!("  {}  \n", std::process::id()));
    assert_eq!(daemon::read_pid(), Some(std::process::id() as i32));
    daemon::clear_pid_file();
}

#[test]
fn our_own_live_pid_counts_as_a_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("live");

    let pid = std::process::id();
    daemon::write_pid(pid);

    assert_eq!(daemon::live_pid(), Some(pid as i32));
    assert_eq!(daemon::read_pid(), Some(pid as i32));

    daemon::clear_pid_file();
    assert_eq!(daemon::live_pid(), None);
}

#[test]
fn writing_the_pid_replaces_whatever_was_recorded_before() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("overwrite");

    daemon::write_pid(999_999);
    daemon::write_pid(std::process::id());

    assert_eq!(daemon::read_pid(), Some(std::process::id() as i32));
    daemon::clear_pid_file();
}

/// The state dir outlives reboots, so a recorded pid can be alive and belong to
/// something else entirely. pid 1 is always alive and is never us.
#[cfg(target_os = "linux")]
#[test]
fn a_reused_pid_belonging_to_another_program_is_not_a_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("reuse");

    write_pid_file("1");

    assert_eq!(
        daemon::live_pid(),
        None,
        "/proc/1/comm is not our binary, so this pid was reused"
    );
    assert!(
        !exists(&config::pid_file()),
        "and the reused marker is swept so --enable can spawn"
    );
}

#[test]
fn a_live_daemon_that_is_not_this_process_still_counts() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("successor");
    let Some(sleeper) = Sleeper::spawn() else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };

    write_pid_file(&sleeper.pid.to_string());

    assert_eq!(daemon::live_pid(), Some(sleeper.pid));
    // The real daemon is never the process asking, so a guard that only accepted
    // our own pid would report every running daemon as dead.
    assert!(exists(&config::pid_file()));
}

#[test]
fn a_successors_marker_is_never_deleted_by_someone_elses_cleanup() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("nosweep");
    let Some(sleeper) = Sleeper::spawn() else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };

    write_pid_file(&sleeper.pid.to_string());
    daemon::clear_pid_file();

    assert!(
        exists(&config::pid_file()),
        "a live daemon of ours owns this marker; deleting it would let a second one start"
    );
    assert_eq!(daemon::read_pid(), Some(sleeper.pid));
}

#[test]
fn an_unwritable_state_dir_is_reported_and_never_fatal() {
    let _guard = env_lock();
    let dirs = TempDirs::new("readonly");
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root ignores directory permissions");
        return;
    }
    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o500)).expect("chmod");

    // Neither call may panic or abort the user's action; both warn on stderr.
    daemon::write_pid(std::process::id());
    daemon::mark_enabled(true);

    assert!(!exists(&config::pid_file()));
    assert!(!daemon::is_enabled());
    assert_eq!(daemon::read_pid(), None);
    assert_eq!(
        daemon::live_pid(),
        None,
        "no marker means no daemon, not a crash"
    );
    // And clearing markers that were never written is still quiet.
    daemon::clear_pid_file();
    daemon::mark_enabled(false);

    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o755))
        .expect("restore");
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

#[test]
fn restore_is_a_no_op_when_never_enabled() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restore-off");

    daemon::restore().expect("restore must stay silent, not fail");

    assert!(
        !exists(&config::pid_file()),
        "restore must not spawn a daemon the user never asked for"
    );
    assert!(!daemon::is_enabled());
}

#[test]
fn restore_is_a_no_op_when_a_daemon_is_already_live() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restore-live");

    daemon::mark_enabled(true);
    // Our own pid stands in for a live daemon, so restore has nothing to do.
    daemon::write_pid(std::process::id());

    daemon::restore().expect("restore");

    assert_eq!(
        daemon::read_pid(),
        Some(std::process::id() as i32),
        "a second daemon would double every badge push"
    );
    daemon::clear_pid_file();
}

#[test]
fn restore_spawns_a_detached_daemon_when_it_was_enabled_and_nothing_is_live() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restore-on");
    daemon::mark_enabled(true);
    assert!(!exists(&config::pid_file()));

    daemon::restore().expect("restore");

    // The detached child is this test binary re-execed with `--daemon`, which it
    // rejects and exits on — enough to prove the spawn happened and the pid was
    // recorded, without a sampler ever touching a socket.
    let pid = daemon::read_pid().expect("restore records the pid it spawned");
    assert_ne!(pid, std::process::id() as i32);
    reap_spawned();
    assert!(!exists(&config::pid_file()));
    assert!(
        daemon::is_enabled(),
        "restore must not disturb the enabled marker"
    );
}

/// Points the platform's unit directory into a temp tree and writes a unit
/// there, so `supervise::is_installed()` is true without touching a real home.
///
/// Both variables, because the directory is `$XDG_CONFIG_HOME/systemd/user` on
/// Linux and `$HOME/Library/LaunchAgents` on macOS.
struct InstalledUnit {
    path: PathBuf,
}

impl InstalledUnit {
    fn new(root: &Path) -> Self {
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg"));
        std::env::set_var("HOME", root.join("home"));
        let path = supervise::unit_path().expect("a supervised platform");
        std::fs::create_dir_all(path.parent().expect("unit dir")).expect("unit dir");
        std::fs::write(&path, "written by a test, never loaded").expect("unit");
        Self { path }
    }
}

impl Drop for InstalledUnit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
    }
}

#[test]
fn restore_leaves_a_supervised_sampler_to_its_supervisor() {
    // herdr's startup hook and the supervisor would otherwise both start a
    // sampler: one detached child and one unit, two processes writing one
    // history file, each rewriting what the other just wrote.
    let _guard = env_lock();
    let dirs = TempDirs::new("restore-supervised");
    let _unit = InstalledUnit::new(&dirs.root);
    daemon::mark_enabled(true);

    daemon::restore().expect("restore");

    assert!(
        !exists(&config::pid_file()),
        "the hook spawned nothing, because the supervisor owns the process"
    );
    assert!(
        daemon::is_enabled(),
        "and the user's choice is untouched: the supervisor is what starts it"
    );
}

#[test]
fn enable_does_not_spawn_a_second_daemon_when_one_is_already_live() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("enable-live");
    daemon::write_pid(std::process::id());

    daemon::enable(&owned(&["--enable"])).expect("enable");

    assert!(
        daemon::is_enabled(),
        "the marker is set first, so a handoff mid-enable still restores"
    );
    assert_eq!(
        daemon::read_pid(),
        Some(std::process::id() as i32),
        "the existing daemon's marker is left exactly as it was"
    );
    daemon::clear_pid_file();
}

#[test]
fn enable_rejects_a_bad_value_before_changing_any_state() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("enable-bad");

    for args in [
        owned(&["--enable", "--interval", "soon"]),
        owned(&["--enable", "--columns", "wide"]),
        owned(&["--enable", "--bucket-seconds"]),
        owned(&["--enable", "--retention-buckets", "-4"]),
    ] {
        let err = daemon::enable(&args).expect_err("a typo'd value must be fatal: {args:?}");

        assert!(
            !err.to_string().is_empty(),
            "the message must name the flag: {err}"
        );
        assert!(
            !daemon::is_enabled(),
            "nothing is marked until the arguments parse: {args:?}"
        );
        assert!(
            !exists(&config::pid_file()),
            "and nothing is spawned either: {args:?}"
        );
    }
}

#[test]
fn disable_clears_the_marker_and_sweeps_a_stale_pid_file() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("disable-stale");
    daemon::mark_enabled(true);
    write_pid_file(&reaped_pid().to_string());

    // The sweep needs a server; there is deliberately none, so disable reports
    // the connection failure. Everything before that must already have happened.
    let err = daemon::disable().expect_err("no herdr to sweep against");

    assert!(err.to_string().contains("cannot reach herdr"), "{err}");
    assert!(
        !daemon::is_enabled(),
        "the marker is cleared first, so nothing mid-teardown concludes a daemon is still wanted"
    );
    assert!(!exists(&config::pid_file()));
}

#[test]
fn disable_stops_a_live_daemon_and_waits_for_it_to_go() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("disable-live");
    let Some(sleeper) = Sleeper::spawn() else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };
    daemon::mark_enabled(true);
    write_pid_file(&sleeper.pid.to_string());

    let started = Instant::now();
    let _ = daemon::disable();

    assert!(
        !is_alive(sleeper.pid),
        "disable must not return while the daemon is still clearing its badges"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a daemon that exits promptly must not cost the whole stop timeout"
    );
    assert!(!daemon::is_enabled());
    assert!(
        !exists(&config::pid_file()),
        "the marker goes once the daemon is gone, so --enable can spawn again"
    );
}

#[test]
fn toggle_stops_a_live_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("toggle-off");
    let Some(sleeper) = Sleeper::spawn() else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };
    daemon::mark_enabled(true);
    write_pid_file(&sleeper.pid.to_string());

    let _ = daemon::toggle(&owned(&["--toggle"]));

    assert!(!is_alive(sleeper.pid));
    assert!(!daemon::is_enabled());
}

#[test]
fn toggle_starts_a_daemon_when_none_is_live() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("toggle-on");

    daemon::toggle(&owned(&["--toggle"])).expect("toggle");

    assert!(daemon::is_enabled());
    assert!(
        daemon::read_pid().is_some(),
        "toggle with nothing running is an enable"
    );
    reap_spawned();
}

#[test]
fn recognised_arguments_are_forwarded_to_the_detached_child() {
    // The child re-reads the config file but never sees the user's command line,
    // so anything typed at `--enable` has to travel with it.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable", "--interval", "30"])).expect("forward"),
        owned(&["--interval", "30"])
    );
    // Both spellings normalise to `--name value`.
    assert_eq!(
        daemon::forwarded_args(&owned(&[
            "--toggle",
            "--interval=30",
            "--bucket-seconds=15",
            "--retention-buckets=100",
            "--columns=12"
        ]))
        .expect("forward"),
        owned(&[
            "--interval",
            "30",
            "--bucket-seconds",
            "15",
            "--retention-buckets",
            "100",
            "--columns",
            "12"
        ])
    );
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable"])).expect("forward"),
        Vec::<String>::new()
    );
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable", "--quiet", "--json"])).expect("forward"),
        Vec::<String>::new(),
        "an unrecognised flag must not reach the child"
    );
    // A value that happens to start with a flag name is still a value.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--columns", "--interval"])).expect("forward"),
        owned(&["--columns", "--interval"])
    );
    // A prefix match is not a flag match.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--intervals", "30"])).expect("forward"),
        Vec::<String>::new()
    );
    // Last occurrence is forwarded too; `config::load_with_args` resolves the
    // duplicate the same way for both parent and child.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--interval", "5", "--interval", "9"])).expect("forward"),
        owned(&["--interval", "5", "--interval", "9"])
    );
    // An empty value is a value, not a missing one.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--interval="])).expect("forward"),
        owned(&["--interval", ""])
    );
    // A reading window is not a recording setting. Forwarding it would make the
    // sampler behave differently for the rest of its life because of how
    // somebody once looked at a pane.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable", "--since", "30m"])).expect("forward"),
        Vec::<String>::new()
    );

    assert!(daemon::forwarded_args(&owned(&["--enable", "--interval"])).is_err());
}

#[test]
fn ttl_is_three_sampling_cycles_and_stays_inside_herdrs_range() {
    let with_interval = |seconds: u64| Config {
        interval: Duration::from_secs(seconds),
        ..Config::default()
    };

    assert_eq!(
        with_interval(5).ttl_ms(),
        15_000,
        "three cycles, so one missed cycle does not blink the badge out"
    );
    assert_eq!(with_interval(MIN_INTERVAL_SECONDS).ttl_ms(), 3_000);
    assert_eq!(
        with_interval(MAX_INTERVAL_SECONDS).ttl_ms(),
        MAX_INTERVAL_SECONDS * 3_000
    );
    assert!(
        with_interval(MAX_INTERVAL_SECONDS).ttl_ms() <= 86_400_000,
        "herdr's ceiling"
    );
    // A zero interval would derive a zero TTL, which herdr rejects outright.
    assert_eq!(with_interval(0).ttl_ms(), 1);
}

// ---------------------------------------------------------------------------
// The badge plan
// ---------------------------------------------------------------------------

#[test]
fn a_tone_flip_clears_the_old_token_before_setting_the_new_one() {
    // Tokens are a merge patch, so a name we do not mention stays lit. Without
    // the clear, herdr renders two badges for one workspace.
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working")]),
        &[badge("w6", Tone::Blocked, "▁▂█ ⏳")],
    );

    assert_eq!(
        plan,
        vec![
            BadgeOp::Clear {
                workspace_id: "w6".to_string(),
                token: "pulse_working".to_string(),
            },
            BadgeOp::Set {
                workspace_id: "w6".to_string(),
                token: "pulse_blocked",
                text: "▁▂█ ⏳".to_string(),
            },
        ],
        "clear first, then set — the order is the whole point"
    );
}

#[test]
fn an_unchanged_tone_is_re_set_so_the_ttl_never_lapses() {
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working")]),
        &[badge("w6", Tone::Working, "▁▂█ ▶")],
    );

    assert_eq!(
        plan,
        vec![BadgeOp::Set {
            workspace_id: "w6".to_string(),
            token: "pulse_working",
            text: "▁▂█ ▶".to_string(),
        }],
        "no redundant clear, but the write still refreshes the TTL"
    );
}

#[test]
fn an_empty_badge_is_a_clear_and_never_a_draw() {
    // `render::badge` returns the empty string when there is nothing worth
    // showing, and an empty token value would occupy the sidebar row with
    // nothing at all.
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working")]),
        &[badge("w6", Tone::Quiet, "")],
    );

    assert_eq!(
        plan,
        vec![BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "pulse_working".to_string(),
        }]
    );

    // A workspace that was never lit costs no calls at all.
    assert!(daemon::plan_for(&HashMap::new(), &[badge("w6", Tone::Quiet, "")]).is_empty());
    // Even when the empty badge carries the same tone that is already lit: the
    // token to clear is the one on record, not the one this cycle computed.
    assert_eq!(
        daemon::plan_for(
            &lit(&[("w6", "pulse_quiet")]),
            &[badge("w6", Tone::Quiet, "")]
        ),
        vec![BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "pulse_quiet".to_string(),
        }]
    );
}

#[test]
fn a_workspace_that_left_the_report_is_cleared_rather_than_left_to_expire() {
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working"), ("w7", "pulse_blocked")]),
        &[badge("w6", Tone::Working, "▁▂█ ▶")],
    );

    assert!(
        plan.contains(&BadgeOp::Clear {
            workspace_id: "w7".to_string(),
            token: "pulse_blocked".to_string(),
        }),
        "a closed workspace must not keep its badge until the TTL expires: {plan:?}"
    );
    // w6 is still reported, so it is refreshed rather than cleared.
    assert!(!plan.contains(&BadgeOp::Clear {
        workspace_id: "w6".to_string(),
        token: "pulse_working".to_string(),
    }));
}

#[test]
fn a_departed_workspace_is_cleared_exactly_once() {
    // The stale sweep and the tone-flip branch must not both fire for the same
    // workspace, or the plan carries a duplicate call every cycle.
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working")]),
        &[badge("w6", Tone::Quiet, "")],
    );

    assert_eq!(
        plan.iter()
            .filter(|op| matches!(op, BadgeOp::Clear { workspace_id, .. } if workspace_id == "w6"))
            .count(),
        1
    );
}

#[test]
fn one_workspaces_tone_does_not_disturb_another() {
    let plan = daemon::plan_for(
        &lit(&[("w6", "pulse_working")]),
        &[
            badge("w6", Tone::Blocked, "█ ⏳"),
            badge("w7", Tone::Working, "▂▃ ▶"),
            badge("w8", Tone::Quiet, ""),
        ],
    );

    let sets = plan
        .iter()
        .filter(|op| matches!(op, BadgeOp::Set { .. }))
        .count();
    assert_eq!(sets, 2, "the empty badge draws nothing, the other two draw");
    assert!(!plan.iter().any(|op| matches!(
        op,
        BadgeOp::Clear { workspace_id, .. } if workspace_id == "w7" || workspace_id == "w8"
    )));
}

#[test]
fn the_plan_is_deterministic_however_the_map_iterates() {
    // The lit map is a HashMap, which iterates in an arbitrary order that varies
    // between runs. A plan that inherited that order would make every failure
    // irreproducible.
    let active = lit(&[
        ("w1", "pulse_quiet"),
        ("w2", "pulse_working"),
        ("w3", "pulse_blocked"),
        ("w4", "pulse_working"),
        ("w5", "pulse_quiet"),
    ]);
    let badges = [badge("w3", Tone::Blocked, "█ ⏳")];

    let first = daemon::plan_for(&active, &badges);
    for _ in 0..25 {
        assert_eq!(daemon::plan_for(&active, &badges), first);
    }

    // And the departed workspaces come out sorted.
    let cleared: Vec<&String> = first
        .iter()
        .filter_map(|op| match op {
            BadgeOp::Clear { workspace_id, .. } => Some(workspace_id),
            BadgeOp::Set { .. } => None,
        })
        .collect();
    assert_eq!(cleared, ["w1", "w2", "w4", "w5"]);
}

#[test]
fn every_tone_maps_to_its_own_token_name() {
    // Severity is encoded in the token name because herdr renders a token's
    // value as flat text and cannot colour by content. Two tones sharing a name
    // would make the user's `fg` settings meaningless.
    let names: Vec<&str> = [Tone::Blocked, Tone::Working, Tone::Quiet]
        .iter()
        .map(|tone| tone.token_name())
        .collect();
    assert_eq!(names, ["pulse_blocked", "pulse_working", "pulse_quiet"]);

    // And the disable sweep knows every name it may ever have to clear.
    for name in names {
        assert!(
            Tone::ALL_TOKENS.contains(&name),
            "{name} would survive --disable"
        );
    }
    assert_eq!(Tone::ALL_TOKENS.len(), 3);
}

// ---------------------------------------------------------------------------
// The store-geometry-meets-renderer-geometry seam
//
// Everything below builds its `WorkspaceActivity` by driving the real store and
// then asking it for activity with **exactly the arguments `daemon::cycle`
// passes**, rather than by hand-assembling a series. That is the whole point of
// these tests: they are the only cover for the seam where the store's geometry
// meets the renderer's, and a hand-built series is free to have a length the
// store can never emit. An earlier version of these tests used 3- and 4-column
// series against the default `badge_columns = 8` — a shape `cycle` cannot
// produce, which made them evidence for nothing.
// ---------------------------------------------------------------------------

/// A session mark with a readable fingerprint for provenance tests.
fn session(fingerprint: &str, began: u64) -> SessionMark {
    SessionMark {
        fingerprint: fingerprint.to_string(),
        began,
    }
}

/// One sample of one workspace, in the shape `herdr::reduce_snapshot` builds.
fn sample_of(workspace_id: &str, state: AgentState, taken_at: u64, seq: u64) -> Sample {
    sample_of_in_session(workspace_id, state, taken_at, seq, None)
}

/// The same sample shape, with explicit session provenance.
fn sample_of_in_session(
    workspace_id: &str,
    state: AgentState,
    taken_at: u64,
    seq: u64,
    session: Option<SessionMark>,
) -> Sample {
    Sample {
        taken_at,
        session,
        workspaces: vec![WorkspaceObservation {
            workspace_id: workspace_id.to_string(),
            label: format!("label-{workspace_id}"),
            checkout_path: Some(format!("/home/dev/repos/{workspace_id}")),
            agents: vec![AgentObservation {
                pane_id: format!("{workspace_id}:p1"),
                workspace_id: workspace_id.to_string(),
                program: Some("claude".to_string()),
                state,
                state_change_seq: seq,
            }],
        }],
    }
}

/// Records `count` samples one `config.interval` apart, ending at `until`.
///
/// `moving` chooses whether the agent's `state_change_seq` advances between
/// samples, which is what separates a bucket with churn in it from a genuinely
/// quiet one.
fn recorded(
    config: &Config,
    workspace_id: &str,
    state: AgentState,
    until: u64,
    count: u64,
    moving: bool,
) -> History {
    let mut history = History::empty(config);
    let step = config.interval.as_secs().max(1);
    for index in 0..count {
        let taken_at = until - (count - 1 - index) * step;
        let seq = if moving { 700 + index } else { 700 };
        history.record(&sample_of(workspace_id, state, taken_at, seq), config);
    }
    history
}

/// The store's answer at exactly the geometry `daemon::cycle` asks for.
fn production_activity(history: &History, config: &Config, as_of: u64) -> Vec<WorkspaceActivity> {
    history.activity(
        as_of,
        config.badge_columns,
        config.buckets_per_badge_column(),
        config,
    )
}

#[test]
fn the_series_the_store_hands_the_daemon_is_exactly_as_wide_as_the_badge() {
    // The invariant the other seam tests rest on, and the one that made
    // `render::badge`'s truncation branch unreachable in production: `cycle`
    // asks for `badge_columns` columns, so the series *is* the window and
    // `badge` never has anything to trim.
    for columns in [1usize, 4, 8, 13, 64] {
        let config = Config {
            badge_columns: columns,
            ..Config::default()
        };
        let as_of = 1_700_000_000;
        let history = recorded(&config, "w6", AgentState::Working, as_of, 12, true);

        let activity = production_activity(&history, &config, as_of);

        assert_eq!(activity.len(), 1);
        assert_eq!(
            activity[0].series.len(),
            config.badge_columns,
            "the store must answer at the width the badge draws"
        );
        // And the badge spends exactly that many columns plus the state glyph.
        assert_eq!(
            pulse::render::badge(&activity[0], &config).chars().count(),
            config.badge_columns + 1
        );
    }
}

#[test]
fn badge_plan_takes_its_token_from_the_tone_and_its_text_from_the_renderer() {
    // `plan_for` is exercised in full above; this is the wiring between it and
    // the two modules that decide what a badge says, which is the only part of
    // `badge_plan` a pure test cannot see.
    let config = Config::default();
    let as_of = 1_700_000_000;
    for (state, token) in [
        (AgentState::Blocked, "pulse_blocked"),
        (AgentState::Working, "pulse_working"),
        (AgentState::Idle, "pulse_quiet"),
        (AgentState::Done, "pulse_quiet"),
        (AgentState::Unknown, "pulse_quiet"),
    ] {
        let history = recorded(&config, "w6", state, as_of, 12, true);
        let activity = production_activity(&history, &config, as_of);
        let expected = pulse::render::badge(&activity[0], &config);
        assert!(
            !expected.is_empty(),
            "a workspace observed for a whole bucket must draw something: {state}"
        );

        let plan = daemon::badge_plan(&HashMap::new(), &activity, None, &config);

        assert_eq!(
            plan,
            vec![BadgeOp::Set {
                workspace_id: "w6".to_string(),
                token,
                text: expected,
            }],
            "state {state}"
        );
    }
}

#[test]
fn a_previous_session_row_is_not_badged_but_the_live_session_row_is() {
    let config = Config::default();
    let as_of = 1_700_000_000;
    let previous = session("previous", as_of - 3_600);
    let live = session("live", as_of - 60);
    let mut history = History::empty(&config);
    history.record(
        &sample_of_in_session(
            "w6",
            AgentState::Blocked,
            as_of,
            700,
            Some(previous.clone()),
        ),
        &config,
    );
    history.record(
        &sample_of_in_session("w6", AgentState::Working, as_of, 701, Some(live.clone())),
        &config,
    );

    let activity = production_activity(&history, &config, as_of);
    assert_eq!(activity.len(), 2, "the store keeps one row per session");
    let previous_row = activity
        .iter()
        .find(|row| row.is_session(Some(&previous)))
        .expect("previous-session row");
    let live_row = activity
        .iter()
        .find(|row| row.is_session(Some(&live)))
        .expect("live-session row");
    assert!(
        !pulse::render::badge(previous_row, &config).is_empty(),
        "the previous row has real observed data; provenance, not blankness, excludes it"
    );
    let live_text = pulse::render::badge(live_row, &config);

    let plan = daemon::badge_plan(&HashMap::new(), &activity, Some(&live), &config);

    assert_eq!(
        plan,
        vec![BadgeOp::Set {
            workspace_id: "w6".to_string(),
            token: "pulse_working",
            text: live_text,
        }],
        "the previous session's blocked badge must not be sent to the reused id"
    );
}

#[test]
fn a_token_lit_before_its_row_stopped_being_a_target_is_cleared_once() {
    let config = Config::default();
    let as_of = 1_700_000_000;
    let previous = session("previous", as_of - 3_600);
    let live = session("live", as_of - 60);
    let mut history = History::empty(&config);
    history.record(
        &sample_of_in_session("w6", AgentState::Blocked, as_of, 700, Some(previous)),
        &config,
    );
    let activity = production_activity(&history, &config, as_of);
    assert!(
        !pulse::render::badge(&activity[0], &config).is_empty(),
        "the clear must come from provenance filtering, not an empty badge"
    );

    let plan = daemon::badge_plan(
        &lit(&[("w6", "pulse_blocked")]),
        &activity,
        Some(&live),
        &config,
    );

    assert_eq!(
        plan,
        vec![BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "pulse_blocked".to_string(),
        }],
        "filtering a formerly badged row must feed the stale-token clear exactly once"
    );
}

#[test]
fn an_unknown_session_row_is_not_a_target_for_a_known_live_session() {
    let config = Config::default();
    let as_of = 1_700_000_000;
    let live = session("live", as_of - 60);
    let mut history = History::empty(&config);
    history.record(
        &sample_of_in_session("w6", AgentState::Working, as_of, 700, None),
        &config,
    );
    let activity = production_activity(&history, &config, as_of);
    assert!(
        !pulse::render::badge(&activity[0], &config).is_empty(),
        "the unknown row is observed data, not a gap to hide"
    );

    let plan = daemon::badge_plan(&HashMap::new(), &activity, Some(&live), &config);

    assert!(
        plan.is_empty(),
        "unknown provenance must remain visibly distinct from the live session"
    );
}

#[test]
fn badge_plan_clears_a_workspace_whose_whole_window_predates_the_badge() {
    let config = Config::default();
    let as_of = 1_700_000_000;
    // Recorded three hours ago: still inside the four-hour ring, so the store
    // keeps reporting the workspace, but entirely outside the 64-minute badge
    // window, so every column of the series is a gap. This is the one route to
    // an empty badge that production can actually take.
    let history = recorded(
        &config,
        "w6",
        AgentState::Working,
        as_of - 3 * 3_600,
        12,
        true,
    );

    let activity = production_activity(&history, &config, as_of);

    assert_eq!(activity.len(), 1, "the store still tracks the workspace");
    assert!(
        activity[0].series.iter().all(Option::is_none),
        "every column older than anything recorded is a gap: {:?}",
        activity[0].series
    );
    assert!(pulse::render::badge(&activity[0], &config).is_empty());

    let plan = daemon::badge_plan(&lit(&[("w6", "pulse_working")]), &activity, None, &config);

    assert_eq!(
        plan,
        vec![BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "pulse_working".to_string(),
        }],
        "an empty badge is a clear, never a row of blanks"
    );
}

#[test]
fn a_quiet_workspace_still_draws_because_quiet_is_an_answer() {
    // The case worth pinning down across the module boundary: a workspace that
    // was observed and did nothing is *not* the same as one we never watched,
    // and only the second may vanish from the sidebar.
    let config = Config::default();
    let as_of = 1_700_000_000;
    // Idle throughout and no sequence movement, so the newest bucket is observed
    // with zero occupancy and zero churn — quiet, not absent.
    let history = recorded(&config, "w6", AgentState::Idle, as_of, 12, false);

    let activity = production_activity(&history, &config, as_of);
    let text = pulse::render::badge(&activity[0], &config);

    assert!(
        activity[0].series.last().expect("newest column").is_some(),
        "the newest column was observed"
    );
    assert!(
        text.contains(pulse::render::QUIET),
        "an observed-and-idle column is the quiet glyph, not a gap: {text:?}"
    );
    assert!(
        !text.is_empty(),
        "an observed-and-idle workspace must keep its row"
    );

    let plan = daemon::badge_plan(&HashMap::new(), &activity, None, &config);

    assert_eq!(
        plan,
        vec![BadgeOp::Set {
            workspace_id: "w6".to_string(),
            token: "pulse_quiet",
            text,
        }]
    );
}

#[test]
fn nothing_reported_and_nothing_lit_is_an_empty_plan() {
    assert!(daemon::plan_for(&HashMap::new(), &[]).is_empty());
}

#[test]
fn every_lit_workspace_is_cleared_when_the_session_empties() {
    // herdr restarted, or every workspace closed at once.
    let plan = daemon::plan_for(&lit(&[("w6", "pulse_working"), ("w7", "pulse_quiet")]), &[]);

    assert_eq!(
        plan,
        vec![
            BadgeOp::Clear {
                workspace_id: "w6".to_string(),
                token: "pulse_working".to_string(),
            },
            BadgeOp::Clear {
                workspace_id: "w7".to_string(),
                token: "pulse_quiet".to_string(),
            },
        ]
    );
}
