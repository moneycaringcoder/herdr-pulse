//! herdr socket client.
//!
//! Owned by the sampler. `model.rs` and `config.rs` are the contract and must
//! not be edited here.
//!
//! Newline-delimited JSON over the socket at `HERDR_SOCKET_PATH`. The server
//! answers exactly one request per connection and then closes, so every call
//! must be able to reconnect and retry once — see `docs/herdr-protocol.md`.
//!
//! # The shape of a snapshot, verified live against herdr 0.8.0 / protocol 19
//!
//! ```text
//! {"id":"...","result":{"type":"session_snapshot","snapshot":{
//!    "version":"0.8.0","protocol":19,
//!    "workspaces":[{workspace_id,label,number,agent_status,focused,
//!                   pane_count,tab_count,active_tab_id,tokens?,worktree?}],
//!    "agents":[{pane_id,workspace_id,tab_id,terminal_id,agent,agent_session,
//!               agent_status,state_change_seq,revision,cwd,focused,tokens,...}],
//!    "panes":[...], "tabs":[...], "layouts":[...]}}}
//! ```
//!
//! The arrays live **one level below** `result`, under `snapshot`. Reading them
//! off the result object yields nothing at all, which looks exactly like an idle
//! session — that is how the reference plugin shipped a silent bug past a green
//! suite, so an absent `snapshot` object must be a loud error here.
//!
//! Two things differ from what the protocol notes in `docs/` describe, both
//! confirmed against a live session: `agents[]` entries carry the **full pane
//! shape** (not a reduced `{pane_id, workspace_id, agent_session, name}`), and
//! there is **no `name` field** on them at all. Use `agent` — the program — as
//! the label.
//!
//! # Where the label actually comes from
//!
//! The captured fixture disagrees with the paragraph above on one point: 15 of
//! its 18 `agents[]` entries *do* carry a `name` (the user's own label for the
//! agent, such as `reviewer`), and one carries a `display_agent`.
//! We still populate [`AgentObservation::program`] from `agent`, because that
//! field is named and documented in `model.rs` as *the program* — filling it
//! with a user label would make the type lie. `name` is deliberately not
//! carried: nothing in this plugin renders it, and a field nobody reads is a
//! field nobody keeps correct.
//!
//! # Durable identity
//!
//! `workspaces[].worktree` is `null` for a workspace that is not open on a
//! checkout, and otherwise carries `checkout_path` — the one field in a snapshot
//! that means the same thing in tomorrow's session, which is what the store
//! keys history on. It is read as an `Option` and never defaulted: a workspace
//! with no worktree has no durable identity, which is a different thing from
//! having one that happens to be empty.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config;
use crate::model::{AgentObservation, AgentState, Sample, SessionMark, WorkspaceObservation};
use crate::Result;

/// Long enough that a busy server is not mistaken for a dead one, short enough
/// that the sampling loop can never wedge behind one call.
pub const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// herdr rejects a `ttl_ms` outside this range with `invalid_metadata_ttl`.
/// Clamping is better than losing the push: a badge with a slightly wrong TTL
/// still renders, whereas a rejected report renders nothing at all.
const MIN_TTL_MS: u64 = 1;
const MAX_TTL_MS: u64 = 86_400_000;

/// herdr accepts at most 16 token names in one `workspace.report_metadata`.
/// Nothing this plugin sends comes close — the badge plan is at most two names
/// per workspace and the disable sweep is three — but chunking costs three lines
/// and turns a future protocol-limit breach into extra round trips rather than
/// into a rejected report.
const MAX_TOKENS_PER_REPORT: usize = 16;

/// A herdr error envelope, carried as a real error type so callers can tell
/// `workspace_not_found` (a workspace closed under us — benign) from a transport
/// failure (we are blind and should say so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

/// Error code from a herdr error envelope, or `None` for a transport failure.
pub fn error_code<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    err.downcast_ref::<HerdrError>().map(|e| e.code.as_str())
}

/// Split so that only transport failures are retried. Retrying a rejected
/// request would just be rejected again, and would double-count against herdr's
/// own error accounting.
enum Failure {
    Transport(String),
    Protocol(HerdrError),
}

#[derive(Debug)]
pub struct Herdr {
    pub socket_path: PathBuf,
    next_id: u64,
}

/// One invocation's stable socket pathname and compatibility classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketTarget {
    pub path: PathBuf,
    pub is_default: bool,
}

impl Herdr {
    /// Dials the already-resolved socket selected for this invocation.
    pub fn connect_at(socket_path: &Path) -> Result<Self> {
        dial(socket_path)?;
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            next_id: 0,
        })
    }

    /// Convenience for callers that do not participate in a longer stateful
    /// operation. Runtime lifecycle code uses [`Self::connect_at`] instead.
    pub fn connect() -> Result<Self> {
        Self::connect_at(&socket_target()?.path)
    }

    /// One `session.snapshot`, reduced to a [`Sample`] stamped with `taken_at`
    /// and with the session it was read from.
    ///
    /// Workspaces are kept whether or not they are git repos and whether or not
    /// they currently have agents — a workspace that lost its agent still has a
    /// history worth showing.
    ///
    /// The session is read on **both** sides of the call and only kept when the
    /// two agree. Neither side is safe alone. [`Self::call`] retries a transport
    /// failure precisely because the first attempt can land on a socket the old
    /// server has just unlinked, so a mark read beforehand can name a session
    /// that did not answer — and a mark read afterwards can name one that had not
    /// started listening when we asked. Disagreement means the socket moved
    /// across the call and pulse cannot say which server replied: that is an
    /// unattributable sample, which the store knows how to record, and it lasts
    /// exactly one cycle.
    pub fn sample(&mut self, taken_at: u64) -> Result<Sample> {
        let before = session_mark(&self.socket_path);
        let result = self.call("session.snapshot", json!({}))?;
        let session = before.filter(|mark| session_mark(&self.socket_path).as_ref() == Some(mark));
        reduce_snapshot(&result, session, taken_at)
    }

    /// Sets one badge token on a workspace, with a TTL so it self-clears if this
    /// process dies. `tokens` is a merge patch: only the named token is touched,
    /// which is why a tone flip has to clear the previous name explicitly.
    pub fn set_badge(
        &mut self,
        workspace_id: &str,
        token: &str,
        value: &str,
        ttl_ms: u64,
    ) -> Result<()> {
        self.report_tokens(workspace_id, &[(token, Some(value))], ttl_ms)
    }

    /// Clears one badge token. Sends a null value and **no** `ttl_ms` — sending
    /// one alongside a delete is rejected.
    pub fn clear_badge(&mut self, workspace_id: &str, token: &str) -> Result<()> {
        self.report_tokens(workspace_id, &[(token, None)], 0)
    }

    /// Reports several token changes for one workspace in one round trip.
    /// `Some` sets a token, `None` clears it.
    ///
    /// Batching is verified against a live 0.8.0 server by readback: one report
    /// may set several names, clear several names, or mix a set and a clear, and
    /// a `ttl_ms` alongside a `null` is accepted rather than rejected. That is
    /// worth using — the disable sweep costs one call per workspace instead of
    /// one per token name, and a tone flip's clear-then-set lands atomically
    /// instead of leaving a one-round-trip window with no badge at all.
    ///
    /// `ttl_ms` is sent only when something is actually being set. A report that
    /// only clears must omit it entirely: herdr rejects a TTL on a pure delete
    /// with `invalid_metadata_ttl`, and there would be nothing for it to apply
    /// to anyway.
    pub fn report_tokens(
        &mut self,
        workspace_id: &str,
        tokens: &[(&str, Option<&str>)],
        ttl_ms: u64,
    ) -> Result<()> {
        // No tokens is not an error, it is simply no work — and a report with an
        // empty patch would be a round trip that changes nothing.
        for chunk in tokens.chunks(MAX_TOKENS_PER_REPORT.max(1)) {
            if chunk.is_empty() {
                continue;
            }
            let mut patch = Map::new();
            let mut sets_anything = false;
            for (token, value) in chunk {
                match value {
                    Some(value) => {
                        sets_anything = true;
                        patch.insert((*token).to_string(), Value::String((*value).to_string()));
                    }
                    None => {
                        patch.insert((*token).to_string(), Value::Null);
                    }
                }
            }

            let mut params = Map::new();
            params.insert("workspace_id".into(), json!(workspace_id));
            params.insert("source".into(), json!(config::plugin_id()));
            params.insert("tokens".into(), Value::Object(patch));
            if sets_anything {
                params.insert("ttl_ms".into(), json!(ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS)));
            }
            self.call("workspace.report_metadata", Value::Object(params))?;
        }
        Ok(())
    }

    pub fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        self.call("notification.show", json!({ "title": title, "body": body }))?;
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("pulse:{}", self.next_id);
        match self.call_once(&id, method, &params) {
            Ok(result) => Ok(result),
            Err(Failure::Protocol(err)) => Err(Box::new(err)),
            // One request per connection is the normal path, not an error path:
            // the server EOFs after answering, so the connection we would reuse
            // is already gone. The same retry carries the client across a
            // `herdr update --handoff`, where the first attempt lands on a socket
            // the old server has just unlinked.
            Err(Failure::Transport(first)) => match self.call_once(&id, method, &params) {
                Ok(result) => Ok(result),
                Err(Failure::Protocol(err)) => Err(Box::new(err)),
                Err(Failure::Transport(second)) => {
                    Err(format!("{method} failed twice: {first}; on retry: {second}").into())
                }
            },
        }
    }

    fn call_once(
        &self,
        id: &str,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, Failure> {
        let stream = dial(&self.socket_path).map_err(|e| Failure::Transport(e.to_string()))?;

        // `params` is mandatory and must be an object — never null, `{}` when
        // empty.
        let params = if params.is_object() {
            params.clone()
        } else {
            Value::Object(Map::new())
        };
        let mut line = serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| Failure::Transport(format!("could not encode request: {e}")))?;
        line.push('\n');

        (&stream)
            .write_all(line.as_bytes())
            .and_then(|()| (&stream).flush())
            .map_err(|e| Failure::Transport(format!("write to {method} failed: {e}")))?;

        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .map_err(|e| Failure::Transport(format!("read of {method} response failed: {e}")))?;
        if response.trim().is_empty() {
            return Err(Failure::Transport(
                "server closed the connection without answering".into(),
            ));
        }

        let value: Value = serde_json::from_str(response.trim_end())
            .map_err(|e| Failure::Transport(format!("malformed response to {method}: {e}")))?;

        // The error envelope is checked before `result`, because an
        // `invalid_request` failure comes back with `"id":""` rather than the id
        // we sent — matching on the id first would classify it as a stray line.
        if let Some(err) = value.get("error") {
            return Err(Failure::Protocol(HerdrError {
                code: text(err, "code").unwrap_or("unknown_error").to_string(),
                message: text(err, "message").unwrap_or("no message").to_string(),
            }));
        }
        match value.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(Failure::Transport(format!(
                "response to {method} carried neither result nor error"
            ))),
        }
    }
}

fn dial(socket_path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("cannot reach herdr at {}: {e}", socket_path.display()))?;
    // Without these a half-open socket parks the sampling loop forever.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// The session behind a socket path, or `None` when it cannot be established.
///
/// herdr publishes no session identity — see [`SessionMark`] — so this is taken
/// from the socket file the server bound: its device and inode identify it among
/// every other socket on the machine, and the moment it came into existence is
/// the moment the server started listening, which is the closest thing to "when
/// this session began" that exists without having watched it start.
///
/// Reads metadata and nothing else: no connection, no subprocess, and nothing
/// written. Cheap enough for `--once` to call on every invocation.
///
/// `None` on any failure, and on a path that is not a socket. An unattributable
/// sample is a fact the store knows how to record; a guessed attribution is not.
pub fn session_mark(socket_path: &Path) -> Option<SessionMark> {
    let metadata = std::fs::metadata(socket_path).ok()?;
    if !metadata.file_type().is_socket() {
        return None;
    }
    // Birth time, not `ctime`. `ctime` is the inode's *status change* time, so
    // anything that touches the socket's metadata while the server is running —
    // a `chmod`, a rename of the path and back — moves it. That would change the
    // fingerprint under a live session, orphan the ring being accumulated, and
    // restart the workspace from an empty sparkline for no reason at all.
    // `Metadata::created` is statx `STATX_BTIME` on Linux and `st_birthtime` on
    // macOS, and both CI platforms have it on their default filesystems.
    //
    // `ctime` remains the fallback for a filesystem that reports no birth time,
    // because a mark that moves on a `chmod` is still better than no attribution
    // at all: the failure it causes is a split, and a split only ever
    // under-claims continuity.
    let (seconds, nanos) = match metadata
        .created()
        .ok()
        .and_then(|born| born.duration_since(std::time::UNIX_EPOCH).ok())
    {
        Some(born) => (born.as_secs(), born.subsec_nanos() as i64),
        None => (metadata.ctime().max(0) as u64, metadata.ctime_nsec().max(0)),
    };
    // The nanoseconds are the load-bearing part, not decoration. A Unix socket
    // is unlinked and re-bound on every restart, and the fresh inode is very
    // often the one just freed — measured on ext4, six rebinds of one path in a
    // second reused the same inode and the same whole second every time. Without
    // sub-second resolution those six restarts read as one session, which is the
    // merge this whole feature exists to refuse.
    Some(SessionMark {
        fingerprint: format!("{}:{}:{seconds}:{nanos}", metadata.dev(), metadata.ino(),),
        began: seconds,
    })
}

/// Reduces a raw `session.snapshot` result object to a [`Sample`].
///
/// Split out from the socket so it can be tested against captured real
/// snapshots with no server involved. `result` is the value of the response's
/// `result` key, i.e. the object containing `type` and `snapshot`.
///
/// Returns an error — never an empty sample — when `snapshot` is absent, so a
/// protocol change is loud rather than looking like an idle session.
///
/// `session` is passed in rather than read here: the reduction is a pure
/// function over a captured snapshot, and the session comes from the socket that
/// answered, which a fixture does not have.
pub fn reduce_snapshot(
    result: &Value,
    session: Option<SessionMark>,
    taken_at: u64,
) -> Result<Sample> {
    // Absent (or non-object) `snapshot` is an error rather than a fallback. An
    // empty workspace list is indistinguishable from an idle session, so
    // silently returning one would hide the breakage exactly the way the
    // reference plugin's original bug did — green suite, blank sidebar.
    let snapshot = result
        .get("snapshot")
        .filter(|snapshot| snapshot.is_object())
        .ok_or_else(|| {
            format!(
                "session.snapshot returned no `snapshot` object (result type `{}`); \
                 the workspace arrays live under result.snapshot, not under result",
                text(result, "type").unwrap_or("missing")
            )
        })?;

    // Agents first, keyed by workspace, so the workspace pass is a lookup rather
    // than a rescan of the whole array per workspace.
    let mut by_workspace: Vec<(String, Vec<AgentObservation>)> = Vec::new();
    for agent in array(snapshot, "agents") {
        // Both ids are required. `workspace_id` is the only way to attribute the
        // agent to anything, and `pane_id` is the key `history` uses to track a
        // single agent's `state_change_seq` across samples — two agents sharing
        // a blank key would silently merge into one.
        let (Some(workspace_id), Some(pane_id)) =
            (text(agent, "workspace_id"), text(agent, "pane_id"))
        else {
            continue;
        };
        let observation = AgentObservation {
            pane_id: pane_id.to_string(),
            workspace_id: workspace_id.to_string(),
            // The program, not the user's label — see the module header.
            program: text(agent, "agent").map(str::to_string),
            // An unrecognised or absent status becomes `Unknown`, which still
            // counts as "an agent is here". Dropping the agent instead would
            // make a future sixth herdr state read as an empty workspace.
            state: AgentState::parse(
                agent
                    .get("agent_status")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            // Absent seq is 0, which compares equal to itself and so records no
            // transition. Inventing a value would manufacture activity.
            state_change_seq: agent
                .get("state_change_seq")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        };
        match by_workspace.iter_mut().find(|(id, _)| id == workspace_id) {
            Some((_, agents)) => agents.push(observation),
            None => by_workspace.push((workspace_id.to_string(), vec![observation])),
        }
    }

    let mut workspaces = Vec::new();
    for workspace in array(snapshot, "workspaces") {
        // Without an id there is nothing to key a history against, and nothing
        // to push a badge to.
        let Some(workspace_id) = text(workspace, "workspace_id") else {
            continue;
        };
        workspaces.push(WorkspaceObservation {
            workspace_id: workspace_id.to_string(),
            // Falling back to the id keeps a label-less workspace from looking
            // renamed on every sample, which still matters: the label is the
            // only identity evidence there is for a workspace herdr reports no
            // worktree for.
            label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
            // `worktree` is null for a workspace that is not on a checkout —
            // seven of the ten in the captured fixture. Absent means "no durable
            // identity", never "a different workspace", so it is an `Option` all
            // the way into the store rather than an empty string.
            checkout_path: workspace
                .get("worktree")
                .and_then(|worktree| text(worktree, "checkout_path"))
                .map(str::to_string),
            agents: by_workspace
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, agents)| agents.clone())
                .unwrap_or_default(),
        });
    }

    Ok(Sample {
        taken_at,
        session,
        workspaces,
    })
}
/// Resolves the socket once. Relative injected paths are made absolute against
/// the invoking process's current directory before any detach or supervision
/// boundary. The path is not canonicalized: the socket may be absent during a
/// restart, and its pathname rather than its inode owns runtime state.
pub fn socket_target() -> Result<SocketTarget> {
    socket_target_with_hint(None)
}

/// Internal detached/supervised daemon resolution. Only the daemon entrypoint
/// honors the parent's namespace hint; public actions always classify their own
/// socket and cannot be redirected across namespaces by a private handoff var.
pub(crate) fn daemon_socket_target() -> Result<SocketTarget> {
    let hint = match config::non_empty_env(config::SOCKET_IS_DEFAULT_ENV).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        Some(value) => {
            return Err(format!(
                "{} must be `0` or `1`, got `{value}`",
                config::SOCKET_IS_DEFAULT_ENV
            )
            .into())
        }
        None => None,
    };
    socket_target_with_hint(hint)
}

fn socket_target_with_hint(default_hint: Option<bool>) -> Result<SocketTarget> {
    if let Some(injected) = config::non_empty_env("HERDR_SOCKET_PATH") {
        let path = absolute_path(PathBuf::from(injected))?;
        let is_default = default_hint.unwrap_or_else(|| {
            default_socket_path()
                .and_then(absolute_path)
                .is_ok_and(|fallback| path == fallback)
        });
        return Ok(SocketTarget { path, is_default });
    }

    let path = absolute_path(default_socket_path()?)?;
    Ok(SocketTarget {
        path,
        is_default: true,
    })
}

/// The resolved pathname alone, retained for short-lived client callers.
pub fn socket_path() -> Result<PathBuf> {
    Ok(socket_target()?.path)
}

fn default_socket_path() -> Result<PathBuf> {
    let config_home = config::non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            config::non_empty_env("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or("HERDR_SOCKET_PATH is unset and neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(config_home.join("herdr").join("herdr.sock"))
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Non-empty string field, since herdr reports absent context as an empty string
/// rather than as a missing key.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], |a| a)
}
