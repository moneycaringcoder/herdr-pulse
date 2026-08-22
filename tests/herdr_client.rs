//! Wire-level tests for the socket client.
//!
//! Every test that talks to a server stands up a real Unix socket in a temp
//! directory and asserts the bytes the client puts on the wire, because the
//! parts of this protocol that bite (mandatory `{}` params, one request per
//! connection, the merge-patch clear with no TTL, the arrays living one level
//! below `result`) are invisible from the Rust API alone.
//!
//! # Why the fixture, and not a hand-written reply
//!
//! Every snapshot reply here is built from `tests/data/snapshot-live.json`, a
//! real `session.snapshot` response captured from a live herdr 0.8.0 server with
//! its values sanitised and its structure left byte-exact. The reference plugin
//! this one is modelled on passed its entire suite while being wrong, because
//! its fake server answered in the shape the client expected rather than the
//! shape herdr actually sends. A fixture is the only fake that cannot drift
//! toward the code it is testing.
//!
//! No running herdr is required, and nothing here touches the user's state.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pulse::herdr::{error_code, reduce_snapshot, session_mark, socket_path, Herdr};
use pulse::model::AgentState;
use serde_json::{json, Value};

const SOURCE: &str = "test.pulse";

/// The captured response, whole: `{"id":..., "result":{"type":..., "snapshot":{...}}}`.
const LIVE_SNAPSHOT: &str = include_str!("data/snapshot-live.json");

fn live_response() -> Value {
    serde_json::from_str(LIVE_SNAPSHOT).expect("the fixture is JSON")
}

/// The `result` object of the captured response — what `reduce_snapshot` takes.
fn live_result() -> Value {
    live_response()["result"].clone()
}

/// `HERDR_SOCKET_PATH` and `HERDR_PLUGIN_ID` are process-global, so the tests
/// that set them have to run one at a time even though cargo runs them on
/// separate threads.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Saves and restores process-global variables, so a test that rewrites `HOME`
/// cannot leak that into the next one.
struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn new(variables: &[&str]) -> Self {
        let saved = variables
            .iter()
            .map(|name| (name.to_string(), std::env::var(name).ok()))
            .collect();
        for name in variables {
            std::env::remove_var(name);
        }
        Self { saved }
    }

    fn set(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
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

/// What the server does with one connection.
#[derive(Clone)]
enum Reply {
    /// Answer, then close — the real server's behaviour.
    Line(String),
    /// Read the request and close without answering, which is what a client sees
    /// when it lands on a socket the server is tearing down.
    Eof,
}

struct TestServer {
    path: PathBuf,
    dir: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(replies: Vec<Reply>) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pulse-wire-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Kept short: a Unix socket path is capped at ~108 bytes.
        let path = dir.join("s.sock");

        let listener = UnixListener::bind(&path).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut replies = replies.into_iter();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking");
                            let mut line = String::new();
                            let mut reader = BufReader::new(&stream);
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                continue;
                            }
                            requests.lock().expect("requests").push(line);
                            match replies.next() {
                                Some(Reply::Line(reply)) => {
                                    let mut stream = &stream;
                                    let _ = stream.write_all(reply.as_bytes());
                                    let _ = stream.write_all(b"\n");
                                    let _ = stream.flush();
                                }
                                // Exhausted or an explicit EOF: just close, the
                                // way herdr closes after answering.
                                Some(Reply::Eof) | None => {}
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            path,
            dir,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Herdr {
        std::env::set_var("HERDR_SOCKET_PATH", &self.path);
        std::env::set_var("HERDR_PLUGIN_ID", SOURCE);
        Herdr::connect().expect("connect")
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }

    /// The single request, parsed, with its raw framing already asserted.
    fn only_request(&self) -> Value {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected one request, got {requests:?}");
        parse_framed(&requests[0])
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One line, newline-terminated, with no trailing framing of its own.
fn parse_framed(raw: &str) -> Value {
    assert!(raw.ends_with('\n'), "request must be newline-terminated");
    assert_eq!(
        raw.matches('\n').count(),
        1,
        "one request per line, got {raw:?}"
    );
    serde_json::from_str(raw.trim_end()).expect("request is JSON")
}

/// Every mutation answers with this, verified live against herdr 0.8.0 for both
/// a set and a clear of `workspace.report_metadata`.
fn ok_reply() -> Reply {
    Reply::Line(json!({"id": "pulse:1", "result": {"type": "ok"}}).to_string())
}

/// `notification.show` does **not** answer `ok`: it reports whether the toast was
/// actually shown, and why not when it was not.
fn notification_reply() -> Reply {
    Reply::Line(
        json!({
            "id": "pulse:1",
            "result": {"type": "notification_show", "shown": true, "reason": "shown"}
        })
        .to_string(),
    )
}

/// The captured response, verbatim. Nothing is trimmed: fields the client
/// ignores are load-bearing here, because a reply carrying only what the client
/// reads cannot catch the client reading the wrong thing.
fn live_reply() -> Reply {
    Reply::Line(live_response().to_string())
}

// ---------------------------------------------------------------------------
// Which session a snapshot came from
// ---------------------------------------------------------------------------

#[test]
fn one_socket_is_one_session_however_often_it_is_asked() {
    let server = TestServer::start(vec![live_reply()]);

    let first = session_mark(&server.path).expect("a bound socket has a session");
    let second = session_mark(&server.path).expect("a bound socket has a session");

    assert_eq!(
        first, second,
        "the mark decides whether a series continues, so it must not wobble between calls"
    );
}

#[test]
fn two_sockets_are_two_sessions() {
    let one = TestServer::start(vec![live_reply()]);
    let other = TestServer::start(vec![live_reply()]);

    let first = session_mark(&one.path).expect("mark");
    let second = session_mark(&other.path).expect("mark");

    assert_ne!(
        first.fingerprint, second.fingerprint,
        "two servers listening are two sessions, and joining their histories would \
         state a continuity that never existed"
    );
}

#[test]
fn a_socket_that_is_not_there_has_no_session() {
    let missing = std::env::temp_dir().join(format!("pulse-absent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&missing);

    assert_eq!(
        session_mark(&missing),
        None,
        "an unattributable sample is a fact the store records; a guessed session is not"
    );
}

#[test]
fn a_regular_file_where_the_socket_should_be_is_not_a_session() {
    let server = TestServer::start(vec![live_reply()]);
    let decoy = server.dir.join("not-a-socket");
    std::fs::write(&decoy, b"this is a file").expect("write the decoy");

    // A path that exists proves nothing on its own. Fingerprinting a plain file
    // would hand every session that ever wrote it the same identity.
    assert_eq!(session_mark(&decoy), None);
}

#[test]
fn the_session_began_when_the_socket_was_created() {
    let server = TestServer::start(vec![live_reply()]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let mark = session_mark(&server.path).expect("mark");

    // Seconds old at most: this socket was bound by `TestServer::start`. The
    // point of the field is that a series can say when its session started
    // without pulse having watched it start.
    assert!(
        mark.began <= now && now - mark.began <= 60,
        "began {} is not the socket's creation time near {now}",
        mark.began
    );
}

#[test]
fn rebinding_one_path_is_a_new_session() {
    // What a herdr restart looks like on disk: the same socket path, unlinked
    // and bound again. The inode is very often the one just freed, so this is
    // the collision the fingerprint has to survive — and the only direction of
    // error that matters, because reading two sessions as one would splice two
    // incomparable series together.
    let dir = std::env::temp_dir().join(format!("pulse-rebind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("s.sock");

    let mut marks = Vec::new();
    for _ in 0..6 {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        marks.push(session_mark(&path).expect("a bound socket has a session"));
        drop(listener);
    }
    let _ = std::fs::remove_dir_all(&dir);

    let mut fingerprints: Vec<&str> = marks.iter().map(|m| m.fingerprint.as_str()).collect();
    fingerprints.sort_unstable();
    fingerprints.dedup();
    assert_eq!(
        fingerprints.len(),
        marks.len(),
        "six rebinds of one path are six sessions, not one: {marks:?}"
    );
}

#[test]
fn touching_the_sockets_metadata_does_not_change_the_session() {
    // A `chmod` on a bound socket moves its `ctime` while the server keeps
    // running. If that moved the mark, the live ring would be orphaned and the
    // workspace would restart from an empty sparkline for no reason.
    let server = TestServer::start(vec![live_reply()]);
    let before = session_mark(&server.path).expect("mark");

    std::fs::set_permissions(&server.path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod the bound socket");

    let after = session_mark(&server.path).expect("mark");
    assert_eq!(
        before, after,
        "the session did not change, so neither may its mark"
    );
}

#[test]
fn a_sample_carries_the_session_of_the_socket_it_was_read_from() {
    let server = TestServer::start(vec![live_reply()]);
    let _lock = env_lock();
    let mut client = server.client();

    let sample = client.sample(1_700_000_000).expect("sample");

    assert_eq!(
        sample.session.as_ref().map(|mark| mark.fingerprint.clone()),
        session_mark(&server.path).map(|mark| mark.fingerprint),
        "the store keys a series on this, so it has to be the session that answered"
    );
    assert_eq!(sample.workspaces.len(), 10, "the fixture's workspaces");
}

/// An error envelope, captured verbatim from a live server.
fn error_reply(id: &str, code: &str, message: &str) -> Reply {
    Reply::Line(json!({"id": id, "error": {"code": code, "message": message}}).to_string())
}

// ---------------------------------------------------------------------------
// Framing and transport
// ---------------------------------------------------------------------------

#[test]
fn request_framing_is_a_single_json_line_with_object_params() {
    let _guard = env_lock();
    let server = TestServer::start(vec![live_reply()]);
    let mut client = server.client();

    client.sample(1_000).expect("snapshot");

    let request = server.only_request();
    assert_eq!(request["method"], "session.snapshot");
    assert!(request["id"].is_string(), "id must be a string");
    // Mandatory and an object even when empty — never null, never absent.
    assert_eq!(request["params"], json!({}));
    assert!(request["params"].is_object());
    assert!(
        request.get("jsonrpc").is_none(),
        "this protocol has no jsonrpc field"
    );
}

#[test]
fn one_request_per_connection_is_survived_by_reconnecting() {
    let _guard = env_lock();
    // The first connection is read and closed without an answer, exactly as a
    // server that has just handed off behaves. The retry must land the call.
    let server = TestServer::start(vec![Reply::Eof, live_reply()]);
    let mut client = server.client();

    let sample = client.sample(7).expect("retry should succeed");

    assert_eq!(sample.workspaces.len(), 10);
    assert_eq!(
        server.requests().len(),
        2,
        "the dropped connection must be retried on a fresh one"
    );
}

#[test]
fn the_retry_reuses_the_same_request_id() {
    let _guard = env_lock();
    let server = TestServer::start(vec![Reply::Eof, ok_reply()]);
    let mut client = server.client();

    client
        .set_badge("wM", "pulse_working", "x", 15_000)
        .expect("set");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        parse_framed(&requests[0])["id"],
        parse_framed(&requests[1])["id"],
        "a retry is the same logical call, not a new one"
    );
}

#[test]
fn a_transport_failure_that_survives_the_retry_is_not_a_herdr_error_code() {
    let _guard = env_lock();
    let server = TestServer::start(vec![Reply::Eof, Reply::Eof]);
    let mut client = server.client();

    let err = client.sample(0).expect_err("both attempts fail");

    assert_eq!(
        error_code(&*err),
        None,
        "callers must be able to tell blindness from rejection"
    );
    assert!(
        err.to_string().contains("failed twice"),
        "the message must say both attempts went: {err}"
    );
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn a_malformed_response_line_is_a_transport_failure_and_is_retried() {
    let _guard = env_lock();
    let server = TestServer::start(vec![
        Reply::Line("{ this is not json".to_string()),
        live_reply(),
    ]);
    let mut client = server.client();

    let sample = client.sample(5).expect("the retry lands");

    assert_eq!(sample.workspaces.len(), 10);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn a_response_with_neither_result_nor_error_is_a_transport_failure() {
    let _guard = env_lock();
    // Twice, so the retry is exhausted and the failure surfaces.
    let reply = || Reply::Line(json!({"id": "pulse:1"}).to_string());
    let server = TestServer::start(vec![reply(), reply()]);
    let mut client = server.client();

    let err = client.sample(0).expect_err("nothing to read");

    assert_eq!(error_code(&*err), None);
    assert_eq!(server.requests().len(), 2);
}

// ---------------------------------------------------------------------------
// Error envelopes — captured verbatim from a live 0.8.0 server
// ---------------------------------------------------------------------------

#[test]
fn a_protocol_error_surfaces_as_a_typed_error_and_is_never_retried() {
    let _guard = env_lock();
    let server = TestServer::start(vec![error_reply(
        "pulse:1",
        "workspace_not_found",
        "workspace nope not found",
    )]);
    let mut client = server.client();

    let err = client
        .set_badge("nope", "pulse_quiet", "·", 15_000)
        .expect_err("an error envelope is a failure");

    assert_eq!(error_code(&*err), Some("workspace_not_found"));
    assert!(err.to_string().contains("workspace nope not found"));
    // A rejected request would just be rejected again, and retrying it would
    // double-count against herdr's own error accounting.
    assert_eq!(
        server.requests().len(),
        1,
        "a rejection is not a transport failure"
    );
}

#[test]
fn every_captured_error_envelope_keeps_its_code_and_message() {
    let _guard = env_lock();
    // Verbatim from a live server. `invalid_request` is the odd one: it comes
    // back with an empty `id` rather than the id we sent, so nothing may key off
    // the echo.
    let captured = [
        ("pulse:1", "workspace_not_found", "workspace nope not found"),
        (
            "pulse:1",
            "invalid_metadata_token",
            "invalid metadata token key: $bad name",
        ),
        (
            "pulse:1",
            "invalid_metadata_ttl",
            "metadata ttl_ms must be at least 1",
        ),
        ("", "invalid_request", "invalid request: ..."),
    ];

    for (id, code, message) in captured {
        let server = TestServer::start(vec![error_reply(id, code, message)]);
        let mut client = server.client();

        let err = client
            .set_badge("wM", "pulse_working", "x", 15_000)
            .expect_err("an error envelope is a failure");

        assert_eq!(error_code(&*err), Some(code), "code for {code}");
        assert!(
            err.to_string().contains(message),
            "message for {code} was {err}"
        );
        assert_eq!(server.requests().len(), 1, "no retry for {code}");
    }
}

#[test]
fn an_error_envelope_with_no_code_still_reads_as_a_rejection() {
    let _guard = env_lock();
    let server = TestServer::start(vec![Reply::Line(
        json!({"id": "pulse:1", "error": {}}).to_string(),
    )]);
    let mut client = server.client();

    let err = client
        .clear_badge("wM", "pulse_quiet")
        .expect_err("rejected");

    // Not `None`: a caller that reads `None` as "transport failure" would
    // redial a server that is answering us perfectly well.
    assert_eq!(error_code(&*err), Some("unknown_error"));
    assert_eq!(server.requests().len(), 1);
}

// ---------------------------------------------------------------------------
// The snapshot shape — the bug this plugin exists not to repeat
// ---------------------------------------------------------------------------

/// The regression test for the shape bug: the arrays live under `snapshot`, and
/// a client reading them off `result` finds nothing while reporting success.
#[test]
fn a_live_session_is_read_from_the_nested_snapshot_object() {
    let _guard = env_lock();
    let server = TestServer::start(vec![live_reply()]);
    let mut client = server.client();

    let sample = client.sample(1_700_000_000).expect("snapshot");

    assert_eq!(
        sample.workspaces.len(),
        10,
        "a live 10-workspace session must not read as idle"
    );
    assert_eq!(
        sample
            .workspaces
            .iter()
            .map(|w| w.agents.len())
            .sum::<usize>(),
        18,
        "all 18 agents in the capture must be attributed"
    );
    assert_eq!(sample.taken_at, 1_700_000_000);
}

/// The other half of the same bug: if the payload ever stops carrying
/// `snapshot`, that must be loud. An empty workspace list is indistinguishable
/// from an idle session, so silently returning one would hide the breakage
/// exactly the way the original bug did.
#[test]
fn arrays_at_the_wrong_level_are_a_loud_error_and_not_an_idle_session() {
    let _guard = env_lock();
    // Every array is present and correct — just one level up, where the buggy
    // client read them from.
    let flattened = live_result()["snapshot"].clone();
    assert!(
        flattened["workspaces"]
            .as_array()
            .is_some_and(|w| !w.is_empty()),
        "the flattened payload must still carry the data, or this proves nothing"
    );
    let mut result = flattened;
    result["type"] = json!("session_snapshot");
    let server = TestServer::start(vec![Reply::Line(
        json!({"id": "pulse:1", "result": result}).to_string(),
    )]);
    let mut client = server.client();

    let err = client
        .sample(0)
        .expect_err("a missing `snapshot` object must not read as an idle session");

    assert!(
        err.to_string().contains("snapshot"),
        "the message must name what is missing: {err}"
    );
    assert_eq!(
        error_code(&*err),
        None,
        "a shape change is ours to fix, not a herdr rejection"
    );
}

#[test]
fn a_snapshot_key_that_is_not_an_object_is_also_a_loud_error() {
    // `null`, a string and an array are all "present" to a naive `get`, and all
    // three would reduce to zero workspaces without complaint.
    for degenerate in [json!(null), json!("session_snapshot"), json!([]), json!(0)] {
        let result = json!({"type": "session_snapshot", "snapshot": degenerate});

        let err = reduce_snapshot(&result, None, 0)
            .expect_err("a non-object snapshot must not read as an idle session");

        assert!(
            err.to_string().contains("snapshot"),
            "for {degenerate}: {err}"
        );
    }
}

#[test]
fn the_error_names_the_result_type_it_actually_saw() {
    let err = reduce_snapshot(&json!({"type": "ok"}), None, 0).expect_err("no snapshot");
    assert!(err.to_string().contains("ok"), "{err}");

    // And says so plainly when even the discriminator is missing.
    let err = reduce_snapshot(&json!({}), None, 0).expect_err("no snapshot");
    assert!(err.to_string().contains("missing"), "{err}");
}

// ---------------------------------------------------------------------------
// Reduction: the captured session, read field by field
// ---------------------------------------------------------------------------

#[test]
fn the_captured_session_reduces_to_its_recorded_workspaces() {
    let sample = reduce_snapshot(&live_result(), None, 42).expect("reduce");

    assert_eq!(sample.taken_at, 42);
    assert_eq!(
        sample
            .workspaces
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["wM", "w3", "w6", "wE", "wY", "w15", "w16", "w1B", "w1C", "w1D"],
        "workspace order follows the wire, so the plan built from it is stable"
    );
    assert_eq!(
        sample
            .workspaces
            .iter()
            .map(|w| w.label.as_str())
            .collect::<Vec<_>>(),
        [
            "workspace-6",
            "workspace-7",
            "workspace-8",
            "workspace-9",
            "workspace-10",
            "workspace-11",
            "workspace-12",
            "workspace-13",
            "workspace-14",
            "workspace-15"
        ]
    );
}

#[test]
fn workspaces_are_kept_whether_or_not_they_are_git_repos() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");

    // The capture has three workspaces with a `worktree` and seven without. A
    // workspace that is not a repo still has agents whose history is worth
    // recording — this plugin is not collide.
    assert_eq!(sample.workspaces.len(), 10);
    for id in ["wM", "w3", "w15"] {
        assert!(
            sample.workspaces.iter().any(|w| w.workspace_id == id),
            "{id} has no worktree and must still be tracked"
        );
    }
}

#[test]
fn the_checkout_path_is_carried_when_the_workspace_has_a_worktree() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");
    let path = |id: &str| {
        sample
            .workspaces
            .iter()
            .find(|w| w.workspace_id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .checkout_path
            .clone()
    };

    // The durable key the store recognises a workspace by across a reused id.
    // Two of these three are linked worktrees of one repo, which is why the
    // checkout path is the field carried and not `repo_root` or `repo_name`:
    // those are equal for both and would make the two workspaces one.
    assert_eq!(
        path("w6").as_deref(),
        Some("/home/dev/repos/project-1"),
        "a workspace on its own checkout"
    );
    assert_eq!(
        path("wE").as_deref(),
        Some("/home/dev/.herdr/worktrees/project-1/fix-media-fetch-throughput")
    );
    assert_eq!(
        path("wY").as_deref(),
        Some("/home/dev/.herdr/worktrees/project-1/fix-mart-promote-budget")
    );

    // `worktree` is null for the other seven. That is the absence of a durable
    // key, not an empty one, so it must arrive as `None` rather than as `""`.
    for id in ["wM", "w3", "w15", "w16", "w1B", "w1C", "w1D"] {
        assert_eq!(path(id), None, "{id} has no worktree");
    }
}

#[test]
fn agents_are_attributed_to_their_own_workspace() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");
    let by_id = |id: &str| {
        sample
            .workspaces
            .iter()
            .find(|w| w.workspace_id == id)
            .unwrap_or_else(|| panic!("{id} missing"))
            .clone()
    };

    let single = by_id("wM");
    assert_eq!(single.agents.len(), 1);
    assert_eq!(single.agents[0].pane_id, "wM:p1");
    assert_eq!(single.agents[0].workspace_id, "wM");
    assert_eq!(single.agents[0].state, AgentState::Idle);
    assert_eq!(single.agents[0].state_change_seq, 795);

    let four = by_id("w15");
    assert_eq!(four.agents.len(), 4);
    assert_eq!(
        four.agents
            .iter()
            .map(|a| a.pane_id.as_str())
            .collect::<Vec<_>>(),
        ["w15:p1", "w15:p2", "w15:p3", "w15:p4"],
        "agent order follows the wire"
    );
}

#[test]
fn the_label_is_the_program_and_never_the_users_agent_name() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");
    let agents: Vec<_> = sample
        .workspaces
        .iter()
        .flat_map(|w| w.agents.iter())
        .collect();

    // 15 of the capture's 18 agents carry a user label of their own in a
    // `name` field. `AgentObservation::program` is documented as the *program*,
    // so it must be `claude`/`opencode` — a field that sometimes holds a program
    // and sometimes a nickname is a field nothing can render honestly.
    let programs: Vec<&str> = agents
        .iter()
        .filter_map(|a| a.program.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(programs, ["claude", "opencode"]);
    assert!(
        agents.iter().all(|a| a.program.is_some()),
        "every agent in the capture names its program"
    );
}

#[test]
fn the_workspace_state_is_the_most_actionable_of_its_agents() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");
    let state = |id: &str| {
        sample
            .workspaces
            .iter()
            .find(|w| w.workspace_id == id)
            .expect("workspace")
            .state()
    };

    // w15 holds three `working` agents and one `idle` one.
    assert_eq!(state("w15"), AgentState::Working);
    // wY holds two `done` agents.
    assert_eq!(state("wY"), AgentState::Done);
    assert_eq!(state("wE"), AgentState::Idle);
}

#[test]
fn the_sequence_counter_is_session_global_and_survives_the_reduction() {
    let sample = reduce_snapshot(&live_result(), None, 0).expect("reduce");
    let seqs: Vec<u64> = sample
        .workspaces
        .iter()
        .flat_map(|w| w.agents.iter())
        .map(|a| a.state_change_seq)
        .collect();

    // Session-global: the counter is stamped at each agent's last transition, so
    // values are unique across the whole session rather than per agent.
    assert_eq!(seqs.len(), 18);
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique.len(), 18, "one shared sequence, not 18 counters");
    assert_eq!(
        sample
            .workspaces
            .iter()
            .find(|w| w.workspace_id == "w15")
            .expect("w15")
            .max_seq(),
        796
    );
}

// ---------------------------------------------------------------------------
// Reduction: degenerate cases the capture does not contain
// ---------------------------------------------------------------------------

/// Builds a `result` object around a hand-made snapshot body, keeping the
/// envelope the capture proves is real.
fn result_with(snapshot: Value) -> Value {
    json!({"type": "session_snapshot", "snapshot": snapshot})
}

#[test]
fn a_genuinely_idle_session_is_an_empty_sample_and_not_an_error() {
    let sample = reduce_snapshot(
        &result_with(json!({"workspaces": [], "agents": []})),
        None,
        9,
    )
    .expect("an empty session is data");

    assert!(sample.workspaces.is_empty());
    assert_eq!(sample.taken_at, 9);

    // Absent arrays are the same thing: the `snapshot` object was there, so we
    // did read the payload, and it really was empty.
    let sample =
        reduce_snapshot(&result_with(json!({"version": "0.8.0"})), None, 9).expect("reduce");
    assert!(sample.workspaces.is_empty());
}

#[test]
fn a_workspace_with_no_agents_is_tracked_and_reads_as_unknown() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "notes"}],
            "agents": []
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(sample.workspaces.len(), 1);
    assert!(sample.workspaces[0].agents.is_empty());
    // Not `Idle`: "nothing is running here" and "something is resting here" are
    // different facts.
    assert_eq!(sample.workspaces[0].state(), AgentState::Unknown);
    assert_eq!(sample.workspaces[0].max_seq(), 0);
}

#[test]
fn a_workspace_without_a_label_falls_back_to_its_id() {
    for workspace in [
        json!({"workspace_id": "w1"}),
        json!({"workspace_id": "w1", "label": ""}),
        json!({"workspace_id": "w1", "label": "   "}),
        json!({"workspace_id": "w1", "label": null}),
    ] {
        let sample = reduce_snapshot(&result_with(json!({"workspaces": [workspace]})), None, 0)
            .expect("reduce");
        // The label is `history`'s guard against workspace-id reuse. A stable
        // fallback keeps that comparison stable; an empty one would make every
        // sample look like a rename and drop the history each cycle.
        assert_eq!(sample.workspaces[0].label, "w1");
    }
}

#[test]
fn a_workspace_with_no_id_is_dropped_because_nothing_can_be_keyed_to_it() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [
                {"label": "nameless"},
                {"workspace_id": "", "label": "empty"},
                {"workspace_id": "w2", "label": "real"}
            ]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(
        sample
            .workspaces
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["w2"]
    );
}

#[test]
fn an_agent_missing_either_id_is_dropped_rather_than_merged() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "working"},
                // No pane id: `history` keys an agent's sequence by pane, so two
                // of these would silently collapse into one agent.
                {"workspace_id": "w1", "agent_status": "working"},
                {"pane_id": "", "workspace_id": "w1", "agent_status": "working"},
                // No workspace id: nothing to attribute it to.
                {"pane_id": "w1:p9", "agent_status": "working"}
            ]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(
        sample.workspaces[0]
            .agents
            .iter()
            .map(|a| a.pane_id.as_str())
            .collect::<Vec<_>>(),
        ["w1:p1"]
    );
}

#[test]
fn an_agent_naming_an_unlisted_workspace_is_dropped() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": [
                {"pane_id": "w9:p1", "workspace_id": "w9", "agent_status": "working"}
            ]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(sample.workspaces.len(), 1);
    assert!(
        sample.workspaces[0].agents.is_empty(),
        "an agent with no workspace has nowhere to be drawn"
    );
}

#[test]
fn every_agent_status_the_server_can_send_round_trips() {
    // The complete enum from the server's own schema.
    let agents: Vec<Value> = ["blocked", "done", "idle", "unknown", "working"]
        .iter()
        .enumerate()
        .map(|(index, status)| {
            json!({
                "pane_id": format!("w1:p{index}"),
                "workspace_id": "w1",
                "agent_status": status,
                "state_change_seq": 100 + index
            })
        })
        .collect();
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": agents
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(
        sample.workspaces[0]
            .agents
            .iter()
            .map(|a| a.state)
            .collect::<Vec<_>>(),
        [
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Idle,
            AgentState::Unknown,
            AgentState::Working
        ]
    );
    // Blocked outranks working: the one agent that will never make progress on
    // its own is the one the workspace has to advertise.
    assert_eq!(sample.workspaces[0].state(), AgentState::Blocked);
}

#[test]
fn an_unparseable_status_keeps_the_agent_as_unknown() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "compacting"},
                {"pane_id": "w1:p2", "workspace_id": "w1", "agent_status": ""},
                {"pane_id": "w1:p3", "workspace_id": "w1"},
                {"pane_id": "w1:p4", "workspace_id": "w1", "agent_status": 7},
                {"pane_id": "w1:p5", "workspace_id": "w1", "agent_status": "WORKING"}
            ]
        })),
        None,
        0,
    )
    .expect("reduce");

    // A sixth herdr state must degrade to "we saw an agent and could not
    // classify it", never to "there was no agent here".
    assert_eq!(sample.workspaces[0].agents.len(), 5);
    assert_eq!(
        sample.workspaces[0]
            .agents
            .iter()
            .map(|a| a.state)
            .collect::<Vec<_>>(),
        [
            AgentState::Unknown,
            AgentState::Unknown,
            AgentState::Unknown,
            AgentState::Unknown,
            // Case-insensitive, per the contract's own parser.
            AgentState::Working
        ]
    );
}

#[test]
fn a_missing_or_odd_sequence_number_never_manufactures_a_transition() {
    let sample = reduce_snapshot(&result_with(json!({
        "workspaces": [{"workspace_id": "w1", "label": "one"}],
        "agents": [
            {"pane_id": "w1:p1", "workspace_id": "w1"},
            {"pane_id": "w1:p2", "workspace_id": "w1", "state_change_seq": null},
            {"pane_id": "w1:p3", "workspace_id": "w1", "state_change_seq": "795"},
            {"pane_id": "w1:p4", "workspace_id": "w1", "state_change_seq": -1},
            {"pane_id": "w1:p5", "workspace_id": "w1", "state_change_seq": 18446744073709551615u64}
        ]
    })), None, 0)
    .expect("reduce");

    assert_eq!(
        sample.workspaces[0]
            .agents
            .iter()
            .map(|a| a.state_change_seq)
            .collect::<Vec<_>>(),
        // Anything unreadable is 0: it compares equal to itself, so the next
        // sample records no transition rather than a fabricated one.
        [0, 0, 0, 0, u64::MAX]
    );
}

#[test]
fn a_program_that_is_absent_or_blank_is_none_rather_than_an_empty_label() {
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": [
                {"pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude"},
                {"pane_id": "w1:p2", "workspace_id": "w1", "agent": ""},
                {"pane_id": "w1:p3", "workspace_id": "w1", "agent": "  opencode  "},
                {"pane_id": "w1:p4", "workspace_id": "w1"}
            ]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(
        sample.workspaces[0]
            .agents
            .iter()
            .map(|a| a.program.as_deref())
            .collect::<Vec<_>>(),
        [Some("claude"), None, Some("opencode"), None]
    );
}

#[test]
fn two_workspaces_sharing_an_id_both_keep_their_agents() {
    // herdr should never send this. If it ever does, dropping one silently would
    // be worse than showing both — the badge push is keyed by id and idempotent.
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [
                {"workspace_id": "w1", "label": "first"},
                {"workspace_id": "w1", "label": "second"}
            ],
            "agents": [{"pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "working"}]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert_eq!(sample.workspaces.len(), 2);
    assert_eq!(sample.workspaces[0].agents.len(), 1);
    assert_eq!(sample.workspaces[1].agents.len(), 1);
}

#[test]
fn only_the_agents_array_is_read_and_never_the_panes_array() {
    // `panes[]` carries an `agent` name but no `state_change_seq`, so an agent
    // recovered from it would report a permanent zero and read as wedged.
    let sample = reduce_snapshot(
        &result_with(json!({
            "workspaces": [{"workspace_id": "w1", "label": "one"}],
            "agents": [],
            "panes": [{
                "pane_id": "w1:p1", "workspace_id": "w1",
                "agent": "claude", "agent_status": "working"
            }]
        })),
        None,
        0,
    )
    .expect("reduce");

    assert!(sample.workspaces[0].agents.is_empty());
}

// ---------------------------------------------------------------------------
// Badge reports
// ---------------------------------------------------------------------------

#[test]
fn set_badge_sends_source_tokens_and_ttl() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client
        .set_badge("w6", "pulse_working", "▁▂▃█ ▶", 15_000)
        .expect("set");

    let params = server.only_request()["params"].clone();
    assert_eq!(
        params,
        json!({
            "workspace_id": "w6",
            "source": SOURCE,
            "tokens": {"pulse_working": "▁▂▃█ ▶"},
            "ttl_ms": 15_000
        })
    );
    // No `$` prefix on the wire: that syntax belongs to herdr's config.toml, and
    // sending it earns `invalid_metadata_token`.
    assert!(!params["tokens"]
        .as_object()
        .expect("tokens object")
        .keys()
        .any(|key| key.starts_with('$')));
}

#[test]
fn unicode_block_elements_survive_the_round_trip_byte_for_byte() {
    let _guard = env_lock();
    // The whole ramp, the gap glyph and the quiet glyph in one value. Verified
    // intact against a live server; the sparkline is the entire product, so a
    // mangled glyph is a shipped bug.
    let badge = "▁▂▃▄▅▆▇█·  ⏳";
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client
        .set_badge("w6", "pulse_blocked", badge, 15_000)
        .expect("set");

    let params = server.only_request()["params"].clone();
    assert_eq!(params["tokens"]["pulse_blocked"], json!(badge));
    assert_eq!(
        params["tokens"]["pulse_blocked"]
            .as_str()
            .expect("string")
            .chars()
            .count(),
        badge.chars().count()
    );
}

#[test]
fn set_badge_clamps_ttl_into_the_protocol_range() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply(), ok_reply()]);
    let mut client = server.client();

    client.set_badge("w6", "pulse_quiet", "·", 0).expect("low");
    client
        .set_badge("w6", "pulse_quiet", "·", u64::MAX)
        .expect("high");

    let requests = server.requests();
    // herdr rejects a TTL outside 1..=86_400_000, and a rejected report renders
    // nothing at all — clamping loses far less than the push does.
    assert_eq!(parse_framed(&requests[0])["params"]["ttl_ms"], 1);
    assert_eq!(
        parse_framed(&requests[1])["params"]["ttl_ms"],
        86_400_000u64
    );
}

#[test]
fn clear_badge_sends_a_null_token_and_no_ttl() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client.clear_badge("w6", "pulse_working").expect("clear");

    let params = server.only_request()["params"].clone();
    // Tokens are a merge patch: null deletes the name, and a TTL alongside a
    // pure delete is rejected with `invalid_metadata_ttl`.
    assert!(params["tokens"]["pulse_working"].is_null());
    assert!(
        params.get("ttl_ms").is_none(),
        "a clear must omit ttl_ms entirely, got {params}"
    );
    assert_eq!(params["source"], SOURCE);
}

#[test]
fn one_report_can_clear_several_tokens_at_once() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client
        .report_tokens(
            "w6",
            &[
                ("pulse_blocked", None),
                ("pulse_working", None),
                ("pulse_quiet", None),
            ],
            15_000,
        )
        .expect("clear all");

    // Verified live by readback: the disable sweep costs one round trip per
    // workspace, not one per token name.
    let params = server.only_request()["params"].clone();
    assert!(params["tokens"]["pulse_blocked"].is_null());
    assert!(params["tokens"]["pulse_working"].is_null());
    assert!(params["tokens"]["pulse_quiet"].is_null());
    assert!(
        params.get("ttl_ms").is_none(),
        "nothing was set, so there is nothing for a TTL to apply to"
    );
}

#[test]
fn one_report_can_mix_a_clear_and_a_set_and_still_carries_a_ttl() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client
        .report_tokens(
            "w6",
            &[("pulse_working", None), ("pulse_blocked", Some("█ ⏳"))],
            15_000,
        )
        .expect("flip");

    // A tone flip in one patch: there is no window where the old badge is gone
    // and the new one has not arrived. `ttl_ms` alongside a null is accepted —
    // verified live, and the reference plugin's notes list it as unknown.
    let params = server.only_request()["params"].clone();
    assert!(params["tokens"]["pulse_working"].is_null());
    assert_eq!(params["tokens"]["pulse_blocked"], json!("█ ⏳"));
    assert_eq!(params["ttl_ms"], 15_000);
}

#[test]
fn a_report_with_nothing_to_change_costs_no_round_trip() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client.report_tokens("w6", &[], 15_000).expect("no-op");

    assert!(
        server.requests().is_empty(),
        "an empty merge patch changes nothing and is not worth a connection"
    );
}

#[test]
fn a_report_larger_than_the_protocol_limit_is_split_rather_than_rejected() {
    let _guard = env_lock();
    let names: Vec<String> = (0..17).map(|index| format!("pulse_t{index}")).collect();
    let tokens: Vec<(&str, Option<&str>)> =
        names.iter().map(|name| (name.as_str(), None)).collect();
    let server = TestServer::start(vec![ok_reply(), ok_reply()]);
    let mut client = server.client();

    client.report_tokens("w6", &tokens, 0).expect("split");

    // herdr caps a report at 16 token names. Nothing this plugin sends comes
    // close, but a split beats a rejection if that ever changes.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        parse_framed(&requests[0])["params"]["tokens"]
            .as_object()
            .expect("tokens")
            .len(),
        16
    );
    assert_eq!(
        parse_framed(&requests[1])["params"]["tokens"]
            .as_object()
            .expect("tokens")
            .len(),
        1
    );
}

#[test]
fn the_source_is_the_plugin_id_so_we_never_clobber_another_plugin() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client.clear_badge("w6", "pulse_quiet").expect("clear");

    assert_eq!(server.only_request()["params"]["source"], SOURCE);
}

#[test]
fn notify_sends_title_and_body() {
    let _guard = env_lock();
    // Not an `ok` envelope: this method reports whether the toast was shown.
    let server = TestServer::start(vec![notification_reply()]);
    let mut client = server.client();

    client.notify("pulse", "3 agents blocked").expect("notify");

    let request = server.only_request();
    assert_eq!(request["method"], "notification.show");
    assert_eq!(request["params"]["title"], "pulse");
    assert_eq!(request["params"]["body"], "3 agents blocked");
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

#[test]
fn connect_reports_the_socket_path_when_there_is_no_server() {
    let _guard = env_lock();
    let env = EnvGuard::new(&["HERDR_SOCKET_PATH"]);
    env.set("HERDR_SOCKET_PATH", "/nonexistent/pulse-test.sock");

    let err = Herdr::connect().expect_err("no server listening");

    assert!(
        err.to_string().contains("/nonexistent/pulse-test.sock"),
        "the message must name the path: {err}"
    );
}

#[test]
fn the_injected_socket_path_wins_over_every_fallback() {
    let _guard = env_lock();
    let env = EnvGuard::new(&["HERDR_SOCKET_PATH", "XDG_CONFIG_HOME", "HOME"]);
    env.set("HOME", "/home/ignored");
    env.set("XDG_CONFIG_HOME", "/xdg/ignored");
    env.set("HERDR_SOCKET_PATH", "/injected/herdr.sock");

    assert_eq!(
        socket_path().expect("path"),
        PathBuf::from("/injected/herdr.sock")
    );
}

#[test]
fn an_empty_socket_variable_counts_as_unset() {
    let _guard = env_lock();
    let env = EnvGuard::new(&["HERDR_SOCKET_PATH", "XDG_CONFIG_HOME", "HOME"]);
    // herdr injects empty strings for absent context, so an empty value must
    // fall through rather than resolving to the current directory.
    env.set("HERDR_SOCKET_PATH", "");
    env.set("XDG_CONFIG_HOME", "/config-root");

    assert_eq!(
        socket_path().expect("path"),
        PathBuf::from("/config-root/herdr/herdr.sock")
    );

    env.set("XDG_CONFIG_HOME", "  ");
    env.set("HOME", "/home/test");
    assert_eq!(
        socket_path().expect("path"),
        PathBuf::from("/home/test/.config/herdr/herdr.sock")
    );
}

#[test]
fn a_process_with_no_home_says_which_variables_it_needed() {
    let _guard = env_lock();
    let _env = EnvGuard::new(&["HERDR_SOCKET_PATH", "XDG_CONFIG_HOME", "HOME"]);

    let err = socket_path().expect_err("nothing to resolve from");

    assert!(err.to_string().contains("HERDR_SOCKET_PATH"), "{err}");
}
