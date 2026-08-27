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
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

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

struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new(names: &[&'static str]) -> Self {
        Self {
            saved: names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }

    fn set(&self, name: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        std::env::set_var(name, value);
    }

    fn remove(&self, name: &'static str) {
        std::env::remove_var(name);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
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

    fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    fn paths(&self) -> config::SessionPaths {
        let paths = config::SessionPaths::resolve().expect("session paths");
        assert!(paths.state_root.starts_with(&self.root));
        paths
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

/// A repeating Herdr socket for the real sampler binary.
///
/// The workspace can change while the server stays bound, which lets the
/// forget regression distinguish history loaded before the action from samples
/// recorded by the restarted daemon afterward.
struct SamplerServer {
    path: PathBuf,
    workspace: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SamplerServer {
    fn start(root: &Path) -> Self {
        Self::start_named(root, "herdr.sock")
    }

    fn start_named(root: &Path, name: &str) -> Self {
        let path = root.join(name);
        let listener = UnixListener::bind(&path).expect("bind sampler socket");
        listener.set_nonblocking(true).expect("nonblocking socket");
        let workspace = Arc::new(Mutex::new("old".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let workspace = Arc::clone(&workspace);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(_) => break,
                    };
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value =
                        serde_json::from_str(line.trim_end()).expect("sampler request");
                    let id = request["id"].clone();
                    let response = match request["method"].as_str() {
                        Some("session.snapshot") => {
                            let workspace = workspace.lock().expect("workspace").clone();
                            json!({
                                "id": id,
                                "result": {
                                    "type": "session_snapshot",
                                    "snapshot": {
                                        "workspaces": [{
                                            "workspace_id": workspace,
                                            "label": format!("label-{workspace}"),
                                            "worktree": {
                                                "checkout_path": format!("/work/{workspace}")
                                            }
                                        }],
                                        "agents": [{
                                            "workspace_id": workspace,
                                            "pane_id": format!("{workspace}:p1"),
                                            "agent": "claude",
                                            "agent_status": "working",
                                            "state_change_seq": 1
                                        }],
                                        "panes": [],
                                        "tabs": [],
                                        "layouts": [],
                                        "focused_workspace_id": workspace,
                                        "focused_tab_id": null,
                                        "focused_pane_id": null
                                    }
                                }
                            })
                        }
                        Some("workspace.report_metadata") => {
                            json!({"id": id, "result": {"type": "ok"}})
                        }
                        method => json!({
                            "id": id,
                            "error": {
                                "code": "unexpected_method",
                                "message": format!("unexpected method {method:?}")
                            }
                        }),
                    };
                    let mut stream = &stream;
                    let _ = writeln!(stream, "{response}");
                    let _ = stream.flush();
                }
            })
        };
        Self {
            path,
            workspace,
            stop,
            thread: Some(thread),
        }
    }

    fn set_workspace(&self, workspace: &str) {
        *self.workspace.lock().expect("workspace") = workspace.to_string();
    }

    fn command(&self, dirs: &TempDirs) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pulse"));
        command
            .env("HERDR_PLUGIN_STATE_DIR", dirs.state())
            .env("HERDR_PLUGIN_CONFIG_DIR", dirs.config())
            .env("HERDR_SOCKET_PATH", &self.path)
            .env("HERDR_PLUGIN_ID", "moneycaringcoder.pulse")
            .env("HOME", dirs.root.join("home"));
        command
    }
}

impl Drop for SamplerServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Owns every sampler pid recorded in the test state directory.
///
/// `--forget` replaces the direct child with a detached grandchild, so cleanup
/// follows the pid marker rather than assuming the process first spawned is
/// still the owner.
struct SamplerProcess {
    initial_reaper: Option<std::thread::JoinHandle<()>>,
    paths: config::SessionPaths,
}

impl SamplerProcess {
    fn new(mut child: Child, paths: config::SessionPaths) -> Self {
        Self {
            // Reap concurrently. A zombie still answers `kill(pid, 0)`, so the
            // separate `pulse --forget` process would otherwise wait out its
            // stop timeout even though the sampler had already exited.
            initial_reaper: Some(std::thread::spawn(move || {
                let _ = child.wait();
            })),
            paths,
        }
    }

    fn detached(paths: config::SessionPaths) -> Self {
        Self {
            initial_reaper: None,
            paths,
        }
    }
}

impl Drop for SamplerProcess {
    fn drop(&mut self) {
        if let Some(pid) = daemon::read_pid(&self.paths) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            if !await_death(pid, Duration::from_secs(2)) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = await_death(pid, Duration::from_secs(2));
            }
        }
        if let Some(reaper) = self.initial_reaper.take() {
            let _ = reaper.join();
        }
        remove_pid_file(&self.paths);
    }
}

fn write_pid_file(paths: &config::SessionPaths, contents: &str) {
    std::fs::create_dir_all(&paths.state_dir).expect("state dir");
    std::fs::write(paths.pid_file(), contents).expect("write pid file");
}

fn remove_pid_file(paths: &config::SessionPaths) {
    let _ = std::fs::remove_file(paths.pid_file());
}

fn hold_owner(paths: &config::SessionPaths) -> std::fs::File {
    std::fs::create_dir_all(&paths.state_dir).expect("state dir");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.owner_lock())
        .expect("owner lock");
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "acquire owner lock"
    );
    file
}

fn session_paths(socket_path: &Path) -> config::SessionPaths {
    config::SessionPaths::for_socket(&pulse::herdr::SocketTarget {
        path: socket_path.to_path_buf(),
        is_default: false,
    })
}

fn owner_is_busy(paths: &config::SessionPaths) -> bool {
    std::fs::create_dir_all(&paths.state_dir).expect("state dir");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.owner_lock())
        .expect("owner lock");
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        false
    } else {
        let code = std::io::Error::last_os_error().raw_os_error();
        assert!(
            code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN),
            "unexpected owner lock failure: {code:?}"
        );
        true
    }
}

fn exists(path: &Path) -> bool {
    path.exists()
}

fn await_file(path: &Path, timeout: Duration, predicate: impl Fn(&str) -> bool) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if predicate(&contents) {
                return Some(contents);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
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
    fn spawn(paths: &config::SessionPaths) -> Option<Self> {
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
        let owner = hold_owner(paths);
        assert_ne!(
            unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_SETFD, 0) },
            -1,
            "make owner lock inheritable"
        );

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
/// `kill(pid, 0)`, which would make later lifecycle calls treat it as a live
/// successor and refuse to remove the marker.
fn reap_spawned(paths: &config::SessionPaths) {
    if let Some(pid) = daemon::read_pid(paths) {
        if pid != std::process::id() as i32 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            await_death(pid, Duration::from_secs(2));
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
        }
    }
    remove_pid_file(paths);
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[test]
fn default_state_is_adopted_in_place_and_named_state_is_collision_free() {
    let _guard = env_lock();
    let dirs = TempDirs::new("session-paths");
    let legacy = config::SessionPaths::for_socket(&pulse::herdr::SocketTarget {
        path: PathBuf::from("/default/herdr.sock"),
        is_default: true,
    });
    assert_eq!(legacy.state_dir, dirs.state());
    assert_eq!(legacy.history_file(), dirs.state().join("history.json"));
    assert_eq!(legacy.supervisor_label(), supervise::LABEL);

    let named = config::SessionPaths::for_socket(&pulse::herdr::SocketTarget {
        path: PathBuf::from("/tmp/named.sock"),
        is_default: false,
    });
    let key = "socket-2f746d702f6e616d65642e736f636b";
    assert_eq!(named.scope_key.as_deref(), Some(key));
    assert_eq!(named.state_dir, dirs.state().join("sessions").join(key));
    assert_eq!(
        named.supervisor_label(),
        format!("{}.{}", supervise::LABEL, key)
    );
}

#[test]
fn remembered_default_socket_survives_later_xdg_changes() {
    let _guard = env_lock();
    let dirs = TempDirs::new("default-marker");
    let env = EnvGuard::new(&["XDG_CONFIG_HOME", "PULSE_SOCKET_IS_DEFAULT"]);
    env.remove("PULSE_SOCKET_IS_DEFAULT");
    let original_config = dirs.root.join("original-config");
    let default_socket = original_config.join("herdr/herdr.sock");
    env.set("XDG_CONFIG_HOME", &original_config);
    std::env::set_var("HERDR_SOCKET_PATH", &default_socket);

    let (first, concurrent) = std::thread::scope(|scope| {
        let other = scope.spawn(|| config::SessionPaths::resolve().map_err(|err| err.to_string()));
        (
            config::SessionPaths::resolve().expect("first default resolution"),
            other
                .join()
                .expect("parallel resolver")
                .expect("concurrent default resolution"),
        )
    });
    assert_eq!(first.state_dir, dirs.state());
    assert_eq!(concurrent.state_dir, dirs.state());
    assert_eq!(
        std::fs::read(dirs.state().join("default.socket")).unwrap(),
        default_socket.as_os_str().as_bytes()
    );

    let different_config = dirs.root.join("different-config");
    env.set("XDG_CONFIG_HOME", &different_config);
    let second = config::SessionPaths::resolve().expect("remembered default resolution");
    assert_eq!(second.state_dir, dirs.state());
    assert!(second.scope_key.is_none());

    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o500))
        .expect("read-only state root");
    let read_only =
        config::SessionPaths::resolve().expect("an existing marker needs no writable lock");
    assert_eq!(read_only.state_dir, dirs.state());
    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o755))
        .expect("restore state root");

    let different_socket = different_config.join("herdr/herdr.sock");
    std::env::set_var("HERDR_SOCKET_PATH", &different_socket);
    let third = config::SessionPaths::resolve().expect("different socket resolution");
    assert_ne!(third.state_dir, dirs.state());
    assert!(third.scope_key.is_some());
    assert_ne!(third.supervisor_label(), supervise::LABEL);
}

#[test]
fn ambiguous_legacy_state_is_reported_instead_of_claimed() {
    let _guard = env_lock();
    let dirs = TempDirs::new("default-ambiguous");
    let env = EnvGuard::new(&["XDG_CONFIG_HOME", "PULSE_SOCKET_IS_DEFAULT"]);
    env.remove("PULSE_SOCKET_IS_DEFAULT");
    env.set("XDG_CONFIG_HOME", dirs.root.join("different-config"));
    std::env::set_var("HERDR_SOCKET_PATH", "/legacy/config/herdr/herdr.sock");
    std::fs::write(dirs.state().join("history.json"), b"legacy").expect("legacy history");

    let err = config::SessionPaths::resolve().expect_err("legacy state is ambiguous");
    assert!(err
        .to_string()
        .contains("cannot assign existing unscoped state"));
    assert!(dirs.state().join("history.json").exists());
}

#[test]
fn internal_daemon_hint_cannot_claim_a_missing_default_marker() {
    let _guard = env_lock();
    let dirs = TempDirs::new("default-internal");
    let env = EnvGuard::new(&["PULSE_SOCKET_IS_DEFAULT"]);
    env.set("PULSE_SOCKET_IS_DEFAULT", "1");
    std::env::set_var(
        "HERDR_SOCKET_PATH",
        dirs.root.join("claimed-default/herdr.sock"),
    );

    let err = config::SessionPaths::resolve_daemon()
        .expect_err("only a public parent may initialize legacy ownership");
    assert!(err.to_string().contains("internal daemon cannot claim"));
    assert!(!dirs.state().join("default.socket").exists());
}

#[test]
fn enabled_flag_round_trips() {
    let _guard = env_lock();
    let dirs = TempDirs::new("enabled");
    let paths = dirs.paths();

    assert!(
        !daemon::is_enabled(&paths),
        "a fresh state dir means never enabled"
    );

    daemon::mark_enabled(&paths, true).expect("enable marker");
    assert!(daemon::is_enabled(&paths));
    assert!(exists(&paths.enabled_flag()));

    daemon::mark_enabled(&paths, false).expect("disable marker");
    assert!(!daemon::is_enabled(&paths));
    assert!(!exists(&paths.enabled_flag()));

    // Disabling twice is a no-op, not an error: the marker is already gone.
    daemon::mark_enabled(&paths, false).expect("disable marker twice");
    assert!(!daemon::is_enabled(&paths));

    // And enabling twice leaves exactly one marker.
    daemon::mark_enabled(&paths, true).expect("enable marker");
    daemon::mark_enabled(&paths, true).expect("enable marker twice");
    assert!(daemon::is_enabled(&paths));
}

// ---------------------------------------------------------------------------
// Why the sampler stopped
// ---------------------------------------------------------------------------

fn write_stop_marker(paths: &config::SessionPaths, contents: &str) {
    std::fs::create_dir_all(&paths.state_dir).expect("state dir");
    std::fs::write(paths.stop_marker(), contents).expect("write stop marker");
}

#[test]
fn a_live_sampler_has_nothing_to_explain() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-live");
    let paths = dirs.paths();
    let _owner = hold_owner(&paths);
    // Our own pid stands in for a live daemon.
    write_pid_file(&paths, &std::process::id().to_string());
    // Even with a marker left over from an earlier run: a gap while a sampler is
    // running means herdr was unreachable, not that the sampler stopped.
    write_stop_marker(&paths, "disabled\n1700000000\n");

    assert!(daemon::stop_report(&paths)
        .expect("read stop report")
        .is_none());

    remove_pid_file(&paths);
}

#[test]
fn a_recorded_stop_is_reported_with_its_reason_and_time() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-recorded");
    let paths = dirs.paths();

    for (written, expected) in [
        ("disabled", daemon::StopReason::Disabled),
        ("terminated", daemon::StopReason::Terminated),
        ("failed", daemon::StopReason::Failed),
    ] {
        write_stop_marker(&paths, &format!("{written}\n1700000000\n"));
        let stop = daemon::stop_report(&paths)
            .expect("read stop report")
            .expect("a stopped sampler");
        assert_eq!(stop.reason, expected, "marker {written:?}");
        assert_eq!(stop.at, Some(1_700_000_000));
        assert_eq!(stop.detail, None);
    }
}

#[test]
fn a_failure_carries_its_one_line_of_detail() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-detail");
    let paths = dirs.paths();
    write_stop_marker(
        &paths,
        "failed\n1700000000\ncannot reach herdr at /nowhere.sock\n",
    );

    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Failed);
    assert_eq!(
        stop.detail.as_deref(),
        Some("cannot reach herdr at /nowhere.sock")
    );
}

#[test]
fn a_run_that_left_no_marker_reads_as_unknown_and_never_as_disabled() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-killed");
    let paths = dirs.paths();
    // What a SIGKILL leaves: the user still wants a sampler, there is not one,
    // and nothing was written on the way out.
    daemon::mark_enabled(&paths, true).expect("enable marker");

    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a sampler that is wanted and absent");
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
    let dirs = TempDirs::new("stop-garbage");
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");

    // A reason from a newer pulse, or a hand-edited file. Either way it is a word
    // this build cannot interpret, and interpreting it anyway is how a guess gets
    // in.
    write_stop_marker(&paths, "evaporated\n1700000000\n");
    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Unknown);
    assert_eq!(
        stop.at,
        Some(1_700_000_000),
        "the marker was parsed, not skipped: without this the enabled-flag \
         fallback would produce the same reason and the test would prove nothing"
    );

    // A marker with no timestamp still names its reason; only the "when"
    // degrades.
    write_stop_marker(&paths, "terminated\n");
    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a stopped sampler");
    assert_eq!(stop.reason, daemon::StopReason::Terminated);
    assert_eq!(stop.at, None);
}

#[test]
fn a_sampler_that_was_never_enabled_here_has_nothing_to_report() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-fresh");
    let paths = dirs.paths();

    // A fresh state dir is not a stopped sampler. Reporting one would put a
    // reason under every empty pane on a machine that has never run `--enable`.
    assert!(daemon::stop_report(&paths)
        .expect("read stop report")
        .is_none());
}

#[test]
fn a_real_panic_string_keeps_its_message_in_the_marker() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stop-panic");
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");

    // What `std` hands a panic hook, verbatim in shape: the location first, the
    // message on the *next* line. A fold that cut at the first newline would
    // keep the file and line and throw the sentence away — in the one place a
    // detached daemon can still say what happened.
    daemon::record_failure(
        &paths,
        "panicked at src/daemon.rs:412:9:\nthe ring length was zero",
    );

    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a stopped sampler");
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
    let dirs = TempDirs::new("stop-huge");
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");

    daemon::record_failure(&paths, &"x".repeat(10_000));

    let stop = daemon::stop_report(&paths)
        .expect("read stop report")
        .expect("a stopped sampler");
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
    let paths = dirs.paths();

    daemon::mark_enabled(&paths, true).expect("enable marker");
    write_pid_file(&paths, &std::process::id().to_string());

    assert!(exists(&paths.enabled_flag()));
    assert_eq!(daemon::read_pid(&paths), Some(std::process::id() as i32));
    remove_pid_file(&paths);
}

#[test]
fn no_pid_file_means_no_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("nopid");
    let paths = dirs.paths();

    assert_eq!(daemon::live_pid(&paths).expect("live pid"), None);
    assert_eq!(daemon::read_pid(&paths), None);
    // Clearing a marker that is not there is not an error.
    remove_pid_file(&paths);
}

#[test]
fn a_stale_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("stale");
    let paths = dirs.paths();

    write_pid_file(&paths, &reaped_pid().to_string());

    assert_eq!(
        daemon::live_pid(&paths).expect("live pid"),
        None,
        "the recorded process has no owner"
    );
    assert!(
        exists(&paths.pid_file()),
        "status reads are non-mutating and cannot race ownership claims"
    );
    daemon::forget_history(&paths).expect("a lifecycle action sweeps stale pid state");
    assert!(!exists(&paths.pid_file()));
}

#[test]
fn a_malformed_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("garbage");
    let paths = dirs.paths();

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
        write_pid_file(&paths, contents);
        assert_eq!(
            daemon::read_pid(&paths),
            None,
            "pid file contents {contents:?}"
        );
        assert_eq!(
            daemon::live_pid(&paths).expect("live pid"),
            None,
            "pid file contents {contents:?}"
        );
        assert!(
            exists(&paths.pid_file()),
            "status reads leave diagnostic files untouched: {contents:?}"
        );
        daemon::forget_history(&paths).expect("lifecycle cleanup");
        assert!(!exists(&paths.pid_file()));
    }

    // Surrounding whitespace is fine — the file is written with `to_string`, but
    // a user may have echoed into it.
    write_pid_file(&paths, &format!("  {}  \n", std::process::id()));
    assert_eq!(daemon::read_pid(&paths), Some(std::process::id() as i32));
    remove_pid_file(&paths);
}

#[test]
fn our_own_live_pid_counts_as_a_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("live");
    let paths = dirs.paths();

    let pid = std::process::id();
    let owner = hold_owner(&paths);
    write_pid_file(&paths, &pid.to_string());

    assert_eq!(
        daemon::live_pid(&paths).expect("live pid"),
        Some(pid as i32)
    );
    assert_eq!(daemon::read_pid(&paths), Some(pid as i32));

    drop(owner);
    remove_pid_file(&paths);
    assert_eq!(daemon::live_pid(&paths).expect("live pid"), None);
}

#[test]
fn writing_the_pid_replaces_whatever_was_recorded_before() {
    let _guard = env_lock();
    let dirs = TempDirs::new("overwrite");
    let paths = dirs.paths();

    write_pid_file(&paths, "999999");
    write_pid_file(&paths, &std::process::id().to_string());

    assert_eq!(daemon::read_pid(&paths), Some(std::process::id() as i32));
    remove_pid_file(&paths);
}

/// The state dir outlives reboots, so a recorded pid without ownership is
/// stale even if the same process number is alive. pid 1 is always alive.
#[cfg(target_os = "linux")]
#[test]
fn a_reused_pid_belonging_to_another_program_is_not_a_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("reuse");
    let paths = dirs.paths();

    write_pid_file(&paths, "1");

    assert_eq!(
        daemon::live_pid(&paths).expect("live pid"),
        None,
        "no sampler owns this session, so this pid is stale"
    );
    assert!(
        exists(&paths.pid_file()),
        "status reads do not mutate lifecycle state"
    );
    daemon::forget_history(&paths).expect("lifecycle cleanup");
    assert!(!exists(&paths.pid_file()));
}

#[test]
fn a_live_daemon_that_is_not_this_process_still_counts() {
    let _guard = env_lock();
    let dirs = TempDirs::new("successor");
    let paths = dirs.paths();
    let Some(sleeper) = Sleeper::spawn(&paths) else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };

    write_pid_file(&paths, &sleeper.pid.to_string());

    assert_eq!(
        daemon::live_pid(&paths).expect("live pid"),
        Some(sleeper.pid)
    );
    // The real daemon is never the process asking, so a guard that only accepted
    // our own pid would report every running daemon as dead.
    assert!(exists(&paths.pid_file()));
}

#[test]
fn a_successors_marker_is_never_deleted_by_someone_elses_cleanup() {
    let _guard = env_lock();
    let dirs = TempDirs::new("nosweep");
    let paths = dirs.paths();
    let Some(sleeper) = Sleeper::spawn(&paths) else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };

    write_pid_file(&paths, &sleeper.pid.to_string());
    daemon::live_pid(&paths).expect("check live marker");

    assert!(
        exists(&paths.pid_file()),
        "a live daemon of ours owns this marker; deleting it would let a second one start"
    );
    assert_eq!(daemon::read_pid(&paths), Some(sleeper.pid));
}

#[test]
fn an_unwritable_state_dir_is_reported_without_panicking() {
    let _guard = env_lock();
    let dirs = TempDirs::new("readonly");
    let paths = dirs.paths();
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root ignores directory permissions");
        return;
    }
    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o500)).expect("chmod");

    // Marker writes remain non-panicking. Status reads do not create/open lock
    // files, so an unwritable empty state reports no published sampler.
    let _ = std::fs::write(paths.pid_file(), std::process::id().to_string());
    let _ = daemon::mark_enabled(&paths, true);

    assert!(!exists(&paths.pid_file()));
    assert!(!daemon::is_enabled(&paths));
    assert_eq!(daemon::read_pid(&paths), None);
    assert_eq!(daemon::live_pid(&paths).expect("status read"), None);
    // And clearing markers that were never written is still quiet.
    remove_pid_file(&paths);
    daemon::mark_enabled(&paths, false).expect("clear missing marker");

    std::fs::set_permissions(dirs.state(), std::fs::Permissions::from_mode(0o755))
        .expect("restore");
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

#[test]
fn restore_is_a_no_op_when_never_enabled() {
    let _guard = env_lock();
    let dirs = TempDirs::new("restore-off");
    let paths = dirs.paths();

    daemon::restore(&paths).expect("restore must stay silent, not fail");

    assert!(
        !exists(&paths.pid_file()),
        "restore must not spawn a daemon the user never asked for"
    );
    assert!(!daemon::is_enabled(&paths));
}

#[test]
fn restore_is_a_no_op_when_a_daemon_is_already_live() {
    let _guard = env_lock();
    let dirs = TempDirs::new("restore-live");
    let paths = dirs.paths();

    daemon::mark_enabled(&paths, true).expect("enable marker");
    let _owner = hold_owner(&paths);
    // Our own pid stands in for a live daemon, so restore has nothing to do.
    write_pid_file(&paths, &std::process::id().to_string());

    daemon::restore(&paths).expect("restore");

    assert_eq!(
        daemon::read_pid(&paths),
        Some(std::process::id() as i32),
        "a second daemon would double every badge push"
    );
    remove_pid_file(&paths);
}

#[test]
fn restore_spawns_a_detached_daemon_when_it_was_enabled_and_nothing_is_live() {
    let _guard = env_lock();
    let dirs = TempDirs::new("restore-on");
    let server = SamplerServer::start(&dirs.root);
    std::env::set_var("HERDR_SOCKET_PATH", &server.path);
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");
    assert!(!exists(&paths.pid_file()));

    let output = server
        .command(&dirs)
        .arg("--restore")
        .output()
        .expect("restore");
    assert!(
        output.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The detached child is this test binary re-execed with `--daemon`, which it
    // rejects and exits on — enough to prove the spawn happened and the pid was
    // recorded, without a sampler ever touching a socket.
    let pid = await_file(&paths.pid_file(), Duration::from_secs(5), |contents| {
        contents.trim().parse::<i32>().is_ok()
    })
    .expect("restore records the pid it spawned")
    .trim()
    .parse::<i32>()
    .expect("pid");
    assert_ne!(pid, std::process::id() as i32);
    reap_spawned(&paths);
    assert!(!exists(&paths.pid_file()));
    assert!(
        daemon::is_enabled(&paths),
        "restore must not disturb the enabled marker"
    );
}

/// Points the platform's unit directory into a temp tree and writes a unit
/// there, so `supervise::is_installed()` is true without touching a real home.
///
/// `HOME` alone: the Linux path asks the running user manager where it looks and
/// falls back to `$HOME/.config/systemd/user`, and the macOS path is
/// `$HOME/Library/LaunchAgents`. Under a temp `HOME` there is no manager to
/// answer, so the fallback is what both platforms use here.
///
/// The previous value is put back on drop rather than removed. `HOME` is not
/// this guard's to delete: every later test in the binary would inherit a
/// process with no home, and the first one to rely on a fallback would resolve
/// somewhere shared across runs.
struct InstalledUnit {
    path: PathBuf,
    previous_home: Option<String>,
}

impl InstalledUnit {
    fn new(root: &Path, paths: &config::SessionPaths) -> Self {
        let previous_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", root.join("home"));
        let path = supervise::unit_path(paths).expect("a supervised platform with a home");
        std::fs::create_dir_all(path.parent().expect("unit dir")).expect("unit dir");
        std::fs::write(&path, "written by a test, never loaded").expect("unit");
        Self {
            path,
            previous_home,
        }
    }
}

impl Drop for InstalledUnit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        match &self.previous_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
fn restore_leaves_a_supervised_sampler_to_its_supervisor() {
    // herdr's startup hook and the supervisor would otherwise both start a
    // sampler: one detached child and one unit, two processes writing one
    // history file, each rewriting what the other just wrote.
    let _guard = env_lock();
    let dirs = TempDirs::new("restore-supervised");
    let paths = dirs.paths();
    let _unit = InstalledUnit::new(&dirs.root, &paths);
    daemon::mark_enabled(&paths, true).expect("enable marker");

    daemon::restore(&paths).expect("restore");

    assert!(
        !exists(&paths.pid_file()),
        "the hook spawned nothing, because the supervisor owns the process"
    );
    assert!(
        daemon::is_enabled(&paths),
        "and the user's choice is untouched: the supervisor is what starts it"
    );
}

#[test]
fn enable_does_not_spawn_a_second_daemon_when_one_is_already_live() {
    let _guard = env_lock();
    let dirs = TempDirs::new("enable-live");
    let paths = dirs.paths();
    let _owner = hold_owner(&paths);
    write_pid_file(&paths, &std::process::id().to_string());

    daemon::enable(&paths, &owned(&["--enable"])).expect("enable");

    assert!(
        daemon::is_enabled(&paths),
        "the marker is set first, so a handoff mid-enable still restores"
    );
    assert_eq!(
        daemon::read_pid(&paths),
        Some(std::process::id() as i32),
        "the existing daemon's marker is left exactly as it was"
    );
    remove_pid_file(&paths);
}

#[test]
fn enable_rejects_a_bad_value_before_changing_any_state() {
    let _guard = env_lock();
    let dirs = TempDirs::new("enable-bad");
    let paths = dirs.paths();

    for args in [
        owned(&["--enable", "--interval", "soon"]),
        owned(&["--enable", "--columns", "wide"]),
        owned(&["--enable", "--bucket-seconds"]),
        owned(&["--enable", "--retention-buckets", "-4"]),
    ] {
        let err =
            daemon::enable(&paths, &args).expect_err("a typo'd value must be fatal: {args:?}");

        assert!(
            !err.to_string().is_empty(),
            "the message must name the flag: {err}"
        );
        assert!(
            !daemon::is_enabled(&paths),
            "nothing is marked until the arguments parse: {args:?}"
        );
        assert!(
            !exists(&paths.pid_file()),
            "and nothing is spawned either: {args:?}"
        );
    }
}

#[test]
fn concurrent_enables_publish_one_same_session_owner() {
    let _guard = env_lock();
    let dirs = TempDirs::new("enable-concurrent");
    std::fs::write(
        dirs.config().join("config.json"),
        r#"{"interval_seconds":1,"bucket_seconds":10}"#,
    )
    .expect("sampler config");
    let server = SamplerServer::start(&dirs.root);
    let paths = session_paths(&server.path);

    let mut first_command = server.command(&dirs);
    first_command
        .arg("--enable")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second_command = server.command(&dirs);
    second_command
        .arg("--enable")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first = first_command.spawn().expect("first enable");
    let second = second_command.spawn().expect("second enable");
    let first = first.wait_with_output().expect("first result");
    let second = second.wait_with_output().expect("second result");
    let _sampler = SamplerProcess::detached(paths.clone());

    assert!(
        first.status.success(),
        "first enable: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "second enable: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(owner_is_busy(&paths), "one daemon retains OWNER");
    assert!(
        daemon::read_pid(&paths).is_some(),
        "the owner published one pid"
    );
    await_file(&paths.history_file(), Duration::from_secs(5), |contents| {
        contents.contains("label-old")
    })
    .expect("the sole owner sampled");
}

#[test]
fn named_sessions_run_and_stop_independently() {
    let _guard = env_lock();
    let dirs = TempDirs::new("named-independent");
    std::fs::write(
        dirs.config().join("config.json"),
        r#"{"interval_seconds":1,"bucket_seconds":10}"#,
    )
    .expect("sampler config");
    let first_server = SamplerServer::start_named(&dirs.root, "first.sock");
    let second_server = SamplerServer::start_named(&dirs.root, "second.sock");
    first_server.set_workspace("first");
    second_server.set_workspace("second");
    let first_paths = session_paths(&first_server.path);
    let second_paths = session_paths(&second_server.path);

    let first = first_server
        .command(&dirs)
        .arg("--enable")
        .output()
        .expect("enable first");
    let second = second_server
        .command(&dirs)
        .arg("--enable")
        .output()
        .expect("enable second");
    let _first_sampler = SamplerProcess::detached(first_paths.clone());
    let _second_sampler = SamplerProcess::detached(second_paths.clone());
    assert!(
        first.status.success(),
        "first session: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "second session: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    assert_ne!(first_paths.state_dir, second_paths.state_dir);
    assert_ne!(
        daemon::read_pid(&first_paths),
        daemon::read_pid(&second_paths)
    );
    assert!(owner_is_busy(&first_paths));
    assert!(owner_is_busy(&second_paths));
    await_file(
        &first_paths.history_file(),
        Duration::from_secs(5),
        |contents| contents.contains("label-first"),
    )
    .expect("first history");
    await_file(
        &second_paths.history_file(),
        Duration::from_secs(5),
        |contents| contents.contains("label-second"),
    )
    .expect("second history");
    let second_pid = daemon::read_pid(&second_paths);

    let disabled = first_server
        .command(&dirs)
        .env("PULSE_SOCKET_IS_DEFAULT", "1")
        .arg("--disable")
        .output()
        .expect("disable first");
    assert!(
        disabled.status.success(),
        "disable first: {}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert!(!owner_is_busy(&first_paths));
    assert!(!daemon::is_enabled(&first_paths));
    assert!(owner_is_busy(&second_paths));
    assert!(daemon::is_enabled(&second_paths));
    assert_eq!(daemon::read_pid(&second_paths), second_pid);
    let second_history =
        std::fs::read_to_string(second_paths.history_file()).expect("second history remains");
    assert!(second_history.contains("label-second"));
    assert!(!second_history.contains("label-first"));
}

#[test]
fn forgetting_while_stopped_does_not_start_or_enable_the_sampler() {
    let _guard = env_lock();
    let dirs = TempDirs::new("forget-stopped");
    let paths = dirs.paths();
    std::fs::create_dir_all(&paths.state_dir).expect("state dir");
    std::fs::write(paths.history_file(), b"recorded").expect("history");

    daemon::forget_history(&paths).expect("forget stopped history");

    assert!(!paths.history_file().exists());
    assert!(!daemon::is_enabled(&paths));
    assert!(!paths.pid_file().exists());
}

#[test]
fn forgetting_while_sampling_cannot_resurrect_the_loaded_history() {
    let _guard = env_lock();
    let dirs = TempDirs::new("forget-live");
    std::fs::write(
        dirs.config().join("config.json"),
        r#"{"interval_seconds":1,"bucket_seconds":10}"#,
    )
    .expect("sampler config");
    let server = SamplerServer::start(&dirs.root);
    std::env::set_var("HERDR_SOCKET_PATH", &server.path);
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");

    let child = server
        .command(&dirs)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start real sampler");
    let _sampler = SamplerProcess::new(child, paths.clone());
    let history_file = paths.history_file();
    await_file(&history_file, Duration::from_secs(5), |contents| {
        contents.contains("label-old") && contents.contains("/work/old")
    })
    .expect("the first daemon persisted its in-memory history");
    let original_pid = daemon::read_pid(&paths).expect("the first daemon recorded its pid");

    server.set_workspace("new");
    let output = server
        .command(&dirs)
        .arg("--forget")
        .output()
        .expect("run forget action");
    assert!(
        output.status.success(),
        "forget failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("sampler restarted"),
        "success is reported only after restart: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let after = await_file(&history_file, Duration::from_secs(5), |contents| {
        contents.contains("label-new") && contents.contains("/work/new")
    })
    .expect("the restarted daemon persisted a new sample");
    assert!(
        !after.contains("label-old") && !after.contains("/work/old"),
        "the pre-forget in-memory store must not return: {after}"
    );
    assert!(
        daemon::is_enabled(&paths),
        "the user's enabled choice survives"
    );
    assert_ne!(
        daemon::read_pid(&paths),
        Some(original_pid),
        "the process holding the old in-memory store was replaced"
    );
}

#[test]
fn disable_clears_the_marker_and_sweeps_a_stale_pid_file() {
    let _guard = env_lock();
    let dirs = TempDirs::new("disable-stale");
    let paths = dirs.paths();
    daemon::mark_enabled(&paths, true).expect("enable marker");
    write_pid_file(&paths, &reaped_pid().to_string());

    // The sweep needs a server; there is deliberately none, so disable reports
    // the connection failure. Everything before that must already have happened.
    let err = daemon::disable(&paths).expect_err("no herdr to sweep against");

    assert!(err.to_string().contains("cannot reach herdr"), "{err}");
    assert!(
        !daemon::is_enabled(&paths),
        "the marker is cleared first, so nothing mid-teardown concludes a daemon is still wanted"
    );
    assert!(!exists(&paths.pid_file()));
}

#[test]
fn disable_stops_a_live_daemon_and_waits_for_it_to_go() {
    let _guard = env_lock();
    let dirs = TempDirs::new("disable-live");
    let paths = dirs.paths();
    let Some(sleeper) = Sleeper::spawn(&paths) else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };
    daemon::mark_enabled(&paths, true).expect("enable marker");
    write_pid_file(&paths, &sleeper.pid.to_string());

    let started = Instant::now();
    let _ = daemon::disable(&paths);

    assert!(
        !is_alive(sleeper.pid),
        "disable must not return while the daemon is still clearing its badges"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a daemon that exits promptly must not cost the whole stop timeout"
    );
    assert!(!daemon::is_enabled(&paths));
    assert!(
        !exists(&paths.pid_file()),
        "the marker goes once the daemon is gone, so --enable can spawn again"
    );
}

#[test]
fn toggle_stops_a_live_daemon() {
    let _guard = env_lock();
    let dirs = TempDirs::new("toggle-off");
    let paths = dirs.paths();
    let Some(sleeper) = Sleeper::spawn(&paths) else {
        eprintln!("skipping: no usable `sleep` binary to stand in for a daemon");
        return;
    };
    daemon::mark_enabled(&paths, true).expect("enable marker");
    write_pid_file(&paths, &sleeper.pid.to_string());

    let _ = daemon::toggle(&paths, &owned(&["--toggle"]));

    assert!(!is_alive(sleeper.pid));
    assert!(!daemon::is_enabled(&paths));
}

#[test]
fn toggle_starts_a_daemon_when_none_is_live() {
    let _guard = env_lock();
    let dirs = TempDirs::new("toggle-on");
    let server = SamplerServer::start(&dirs.root);
    std::env::set_var("HERDR_SOCKET_PATH", &server.path);
    let paths = dirs.paths();

    let output = server
        .command(&dirs)
        .arg("--toggle")
        .output()
        .expect("toggle");
    assert!(
        output.status.success(),
        "toggle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(daemon::is_enabled(&paths));
    assert!(
        daemon::read_pid(&paths).is_some(),
        "toggle with nothing running is an enable"
    );
    reap_spawned(&paths);
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
