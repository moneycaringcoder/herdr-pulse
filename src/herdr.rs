//! herdr socket client.
//!
//! Owned by the sampler. `model.rs` and `config.rs` are the contract and must
//! not be edited here.
//!
//! Newline-delimited JSON over the socket at `HERDR_SOCKET_PATH`. Crook opens
//! one connection per request, matching the server's unary protocol, and
//! retries only requests explicitly marked idempotent.
//!
//! # Snapshot shape, captured at both supported protocol generations
//!
//! ```text
//! {"id":"...","result":{"type":"session_snapshot","snapshot":{
//!    "version":"0.8.x","protocol":19 | 20,
//!    "workspaces":[{workspace_id,label,number,agent_status,focused,
//!                   pane_count,tab_count,active_tab_id,tokens?,worktree?}],
//!    "agents":[{pane_id,workspace_id,tab_id,terminal_id,agent,agent_session,
//!               agent_status,state_change_seq?,revision,cwd,focused,tokens,...}],
//!    "panes":[...], "tabs":[...], "layouts":[...]}}}
//! ```
//!
//! The arrays live **one level below** `result`, under `snapshot`. Reading them
//! off the result object yields nothing at all, which looks exactly like an idle
//! session — that is how the reference plugin shipped a silent bug past a green
//! suite, so an absent `snapshot` object must be a loud error here.
//!
//! Captures from Herdr 0.8.0/protocol 19 and 0.8.2/protocol 20 both show that
//! `agents[]` entries carry the full pane shape, not a reduced agent record.
//! Optional user labels vary by capture and entry; `agent` is the stable program
//! field used for [`AgentObservation::program`]. User `name`/`display_agent`
//! values are deliberately not retained because this plugin renders neither.
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
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crook::client::{Client, Error as CrookError, RetrySafety};
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

#[derive(Debug)]
pub struct Herdr {
    pub socket_path: PathBuf,
    client: Client,
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
        let client = Client::connect(socket_path, "pulse").map_err(map_crook_error)?;
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            client,
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
    /// two agree. Neither side is safe alone. The idempotent Crook request
    /// retries a transport failure precisely because the first attempt can land
    /// on a socket the old server has just unlinked, so a mark read beforehand
    /// can name a session that did not answer — and a mark read afterwards can
    /// name one that had not started listening when we asked. Disagreement means
    /// the socket moved across the call and pulse cannot say which server
    /// replied: that is an unattributable sample, which the store knows how to
    /// record, and it lasts exactly one cycle.
    pub fn sample(&mut self, taken_at: u64) -> Result<Sample> {
        let before = session_mark(&self.socket_path);
        let result = self
            .client
            .request("session.snapshot", json!({}), RetrySafety::Idempotent)
            .map_err(map_crook_error)?;
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
            self.client
                .request(
                    "workspace.report_metadata",
                    Value::Object(params),
                    RetrySafety::Never,
                )
                .map_err(map_crook_error)?;
        }
        Ok(())
    }

    pub fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        self.client
            .request(
                "notification.show",
                json!({ "title": title, "body": body }),
                RetrySafety::Never,
            )
            .map_err(map_crook_error)?;
        Ok(())
    }
}

fn map_crook_error(error: CrookError) -> Box<dyn std::error::Error> {
    match error {
        CrookError::Protocol { code, message } => Box::new(HerdrError { code, message }),
        error => Box::new(error),
    }
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

    let agents = required_array(snapshot, "agents")?;
    let workspace_records = required_array(snapshot, "workspaces")?;

    // Agents first, keyed by workspace, so the workspace pass is a lookup
    // rather than a rescan of the whole array per workspace.
    let mut by_workspace: Vec<(String, Vec<AgentObservation>)> = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        require_object(agent, "agents", index)?;
        let workspace_id = require_text(agent, "agents", index, "workspace_id")?;
        let pane_id = require_text(agent, "agents", index, "pane_id")?;
        let status = require_text(agent, "agents", index, "agent_status")?;
        let observation = AgentObservation {
            pane_id: pane_id.to_string(),
            workspace_id: workspace_id.to_string(),
            // Optional display/program evidence remains forward-compatible.
            program: text(agent, "agent").map(str::to_string),
            // A future non-empty enum member stays present as Unknown. Missing,
            // wrong-type, or blank status was rejected above.
            state: AgentState::parse(status),
            // Optional in Herdr's schema. Absence or an unreadable value cannot
            // justify inventing a transition, so it remains neutral zero.
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
    for (index, workspace) in workspace_records.iter().enumerate() {
        require_object(workspace, "workspaces", index)?;
        let workspace_id = require_text(workspace, "workspaces", index, "workspace_id")?;
        let label = require_text(workspace, "workspaces", index, "label")?;
        let checkout_path = match workspace.get("worktree") {
            None | Some(Value::Null) => None,
            Some(worktree) if worktree.is_object() => {
                let path = text(worktree, "checkout_path").ok_or_else(|| {
                    format!(
                        "session.snapshot.workspaces[{index}].worktree.checkout_path must be a \
                         non-empty string"
                    )
                })?;
                Some(path.to_string())
            }
            Some(_) => {
                return Err(format!(
                    "session.snapshot.workspaces[{index}].worktree must be an object or null"
                )
                .into())
            }
        };
        workspaces.push(WorkspaceObservation {
            workspace_id: workspace_id.to_string(),
            label: label.to_string(),
            checkout_path,
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

fn required_array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("session.snapshot.{key} must be an array").into())
}

fn require_object(value: &Value, kind: &str, index: usize) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(format!("session.snapshot.{kind}[{index}] must be an object").into())
    }
}

fn require_text<'a>(value: &'a Value, kind: &str, index: usize, key: &str) -> Result<&'a str> {
    text(value, key).ok_or_else(|| {
        format!("session.snapshot.{kind}[{index}].{key} must be a non-empty string").into()
    })
}
