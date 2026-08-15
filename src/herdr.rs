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

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::model::Sample;
use crate::Result;

/// Long enough that a busy server is not mistaken for a dead one, short enough
/// that the sampling loop can never wedge behind one call.
pub const IO_TIMEOUT: Duration = Duration::from_secs(15);

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
    next_id: u64,
}

impl Herdr {
    /// Dials once so a missing server is reported here, with the path, rather
    /// than as a confusing failure inside the first call.
    pub fn connect() -> Result<Self> {
        let _ = &Self {
            socket_path: PathBuf::new(),
            next_id: 0,
        };
        todo!("sampler")
    }

    /// One `session.snapshot`, reduced to a [`Sample`] stamped with `taken_at`.
    ///
    /// Workspaces are kept whether or not they are git repos and whether or not
    /// they currently have agents — a workspace that lost its agent still has a
    /// history worth showing.
    pub fn sample(&mut self, _taken_at: u64) -> Result<Sample> {
        todo!("sampler")
    }

    /// Sets one badge token on a workspace, with a TTL so it self-clears if this
    /// process dies. `tokens` is a merge patch: only the named token is touched,
    /// which is why a tone flip has to clear the previous name explicitly.
    pub fn set_badge(
        &mut self,
        _workspace_id: &str,
        _token: &str,
        _value: &str,
        _ttl_ms: u64,
    ) -> Result<()> {
        todo!("sampler")
    }

    /// Clears one badge token. Sends a null value and **no** `ttl_ms` — sending
    /// one alongside a delete is rejected.
    pub fn clear_badge(&mut self, _workspace_id: &str, _token: &str) -> Result<()> {
        todo!("sampler")
    }

    pub fn notify(&mut self, _title: &str, _body: &str) -> Result<()> {
        todo!("sampler")
    }
}

/// Reduces a raw `session.snapshot` result object to a [`Sample`].
///
/// Split out from the socket so it can be tested against captured real
/// snapshots with no server involved. `result` is the value of the response's
/// `result` key, i.e. the object containing `type` and `snapshot`.
///
/// Returns an error — never an empty sample — when `snapshot` is absent, so a
/// protocol change is loud rather than looking like an idle session.
pub fn reduce_snapshot(_result: &Value, _taken_at: u64) -> Result<Sample> {
    todo!("sampler")
}

/// Resolves the socket path: `HERDR_SOCKET_PATH`, else
/// `$XDG_CONFIG_HOME/herdr/herdr.sock`. An empty environment variable counts as
/// unset, because herdr injects empty strings for absent context.
pub fn socket_path() -> Result<PathBuf> {
    todo!("sampler")
}
