//! Optional supervision: a systemd user unit on Linux, a launchd agent on macOS.
//!
//! The sampler is a detached process that outlives herdr and dies with the
//! machine. `pulse --supervise` hands its lifecycle to the platform's own
//! supervisor so that "always watching" survives a reboot, and
//! `pulse --unsupervise` hands it back.
//!
//! # What supervision does not change
//!
//! Nothing about what is recorded, and nothing about how a gap is judged. The
//! supervised process is the same `pulse --daemon` with the same config, writing
//! the same buckets to the same file. In particular every bucket a restarted
//! sampler did not observe stays a gap: **continuity of the unit is not
//! continuity of observation**, and a supervisor that quietly papered over its
//! own downtime would be drawing zeros for minutes nobody watched.
//!
//! # Why the environment is baked into the unit
//!
//! herdr injects `HERDR_PLUGIN_STATE_DIR` and `HERDR_SOCKET_PATH` into the
//! commands it spawns. A supervisor spawns the sampler with neither, so the unit
//! carries the paths this machine resolves *now*, written out in full. Letting
//! the supervised sampler re-derive them would work until the day a variable
//! changes underneath it, and then it would record history into a directory the
//! panes never read — a plugin that looks like it is working and is not.
//!
//! # Two shapes of command
//!
//! Every step says whether a failure is fatal. Removing supervision has to work
//! when the unit was already stopped by hand, and bootstrapping a launchd agent
//! has to work when a stale copy is still loaded — so those steps tolerate a
//! non-zero exit, while the steps that actually install and start do not.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{non_empty_env, SessionPaths};
use crate::Result;

/// The unit's name on both platforms. A reverse-DNS label is what launchd wants;
/// systemd takes the same string and adds the suffix.
pub const LABEL: &str = "dev.herdr.pulse.sampler";

/// Seconds a supervisor waits before restarting a sampler that exited.
///
/// Long enough that a sampler failing on every start does not spin, short enough
/// that the gap it leaves is one bucket at the default width rather than a
/// visible hole in the row.
pub const RESTART_SECONDS: u64 = 5;

/// Which supervisor a plan targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervisor {
    Systemd,
    Launchd,
}

impl Supervisor {
    /// The supervisor for the platform this binary was built for, or `None`
    /// where there is nothing to hand the sampler to.
    ///
    /// Compile-time rather than probed: a Linux box with `systemctl` missing is
    /// a machine where this feature does not work, and saying so beats writing a
    /// unit file nothing will ever read.
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::Systemd)
        } else if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else {
            None
        }
    }
    pub fn unit_name(self, label: &str) -> String {
        match self {
            Self::Systemd => format!("{label}.service"),
            Self::Launchd => format!("{label}.plist"),
        }
    }
}

/// One command, and whether its failure stops everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub argv: Vec<String>,
    /// A step that may fail without the whole operation having failed: clearing
    /// a stale registration, or stopping something already stopped.
    pub tolerated: bool,
}

impl Step {
    fn required(parts: &[&str]) -> Self {
        Self {
            argv: parts.iter().map(|part| (*part).to_string()).collect(),
            tolerated: false,
        }
    }

    fn tolerated(parts: &[&str]) -> Self {
        Self {
            tolerated: true,
            ..Self::required(parts)
        }
    }
}

/// Everything an install writes and runs, computed before anything happens.
///
/// Separate from the doing so the unit text and the commands can be tested
/// without a systemd on the machine and without touching a real `~/.config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub supervisor: Supervisor,
    /// Where the unit file goes.
    pub path: PathBuf,
    /// What the unit file says.
    pub contents: String,
    /// Steps that install and start it, in order.
    pub activate: Vec<Step>,
    /// Steps that stop it and take it out of the boot sequence, in order. The
    /// file itself is removed separately, so a stop can be undone.
    pub deactivate: Vec<Step>,
}

/// What the supervised sampler needs handed to it, resolved on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub exe: PathBuf,
    /// The global state root; the daemon applies session scoping exactly once.
    pub state_root: PathBuf,
    pub socket_path: PathBuf,
    pub socket_is_default: bool,
    pub label: String,
    pub args: Vec<String>,
    pub unit_dir: PathBuf,
    pub uid: u32,
}

impl Environment {
    /// This machine and one already-resolved socket namespace.
    pub fn current(paths: &SessionPaths, args: Vec<String>) -> Result<Self> {
        let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
        Ok(Self {
            exe: std::env::current_exe()?,
            state_root: paths.state_root.clone(),
            socket_path: paths.socket_path.clone(),
            socket_is_default: paths.scope_key.is_none(),
            label: paths.supervisor_label(),
            args,
            unit_dir: unit_dir(supervisor)?,
            uid: uid(),
        })
    }
}

const UNSUPPORTED: &str =
    "supervision is available on Linux (systemd) and macOS (launchd) only; on \
     other systems `pulse --enable` still runs the sampler until the machine stops";

const NO_HOME: &str = "cannot place a supervision unit: HOME is unset or not absolute. \
                       Writing one relative to the working directory would put it in \
                       whatever repository you happen to be standing in, and would make \
                       `--enable` and `--restore` answer differently depending on where \
                       they were run";

/// Where a unit file lives, per platform convention.
///
/// The systemd half asks the running user manager rather than deriving a path,
/// and this is not fussiness. A user manager is started by logind long before a
/// shell exists, so it does not inherit an `XDG_CONFIG_HOME` set in a shell rc.
/// Deriving `$XDG_CONFIG_HOME/systemd/user` from *our* environment therefore
/// writes the unit somewhere the manager never reads, and `enable` fails with
/// "unit does not exist" while a perfectly good file sits on disk. Observed on
/// the machine this was written on.
fn unit_dir(supervisor: Supervisor) -> Result<PathBuf> {
    match supervisor {
        Supervisor::Systemd => systemd_unit_dir(),
        Supervisor::Launchd => Ok(home()?.join("Library").join("LaunchAgents")),
    }
}

/// The manager's own per-user unit directory, from `systemctl --user show`.
///
/// `UnitPath` lists every directory the manager searches. The one wanted is the
/// writable per-user config directory — `user.control` and the generator
/// directories are systemd's own scratch space and a hand-written unit does not
/// belong in them. Falls back to the documented default when there is no manager
/// to ask, which is also the case where enabling would fail anyway, and where
/// the error is worth more than a guess.
fn systemd_unit_dir() -> Result<PathBuf> {
    let home = home()?;
    let default = home.join(".config").join("systemd").join("user");
    let Ok(shown) = capture(&Step::required(&[
        "systemctl",
        "--user",
        "show",
        "--property=UnitPath",
    ])) else {
        return Ok(default);
    };
    Ok(unit_path_entries(&shown)
        .into_iter()
        .find(|path| path.starts_with(&home) && path.ends_with("systemd/user"))
        .unwrap_or(default))
}

/// The directories in a `UnitPath=` line.
///
/// systemd quotes any entry containing a space, so splitting on whitespace
/// shreds a home directory with a space in it and silently falls back to the
/// default — which is the one case this function exists to get right.
fn unit_path_entries(shown: &str) -> Vec<PathBuf> {
    let list = shown
        .trim()
        .strip_prefix("UnitPath=")
        .unwrap_or_default()
        .trim();
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in list.chars() {
        match ch {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !current.is_empty() {
                    entries.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        entries.push(PathBuf::from(current));
    }
    entries
}

/// The user's home, refused unless it is absolute.
///
/// `config::state_dir` can fall back to a temp directory when there is no home,
/// because a history file somewhere odd is still a history file. A unit file has
/// no such fallback: a supervisor reads one fixed set of directories, and a copy
/// anywhere else is litter that also makes [`is_installed`] answer differently
/// from different working directories.
fn home() -> Result<PathBuf> {
    non_empty_env("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| NO_HOME.into())
}

fn uid() -> u32 {
    // SAFETY: `getuid` takes no arguments, touches no memory, and cannot fail.
    unsafe { libc::getuid() }
}

/// The plan for an environment, for the supervisor this build targets.
pub fn plan(env: &Environment) -> Result<Plan> {
    plan_for(Supervisor::current().ok_or(UNSUPPORTED)?, env)
}

/// [`plan`] for a named supervisor. Pure: it writes nothing and runs nothing.
///
/// Explicit rather than implied by the target, so the platform this build is not
/// running on is still exercised by tests. The launchd plist was previously
/// unreachable on Linux, which is how a `bootout` that does not survive a reboot
/// got as far as review.
pub fn plan_for(supervisor: Supervisor, env: &Environment) -> Result<Plan> {
    let path = env.unit_dir.join(supervisor.unit_name(&env.label));
    let contents = match supervisor {
        Supervisor::Systemd => systemd_unit(env)?,
        Supervisor::Launchd => launchd_plist(env)?,
    };
    let (activate, deactivate) = steps(supervisor, env.uid, &env.label, &path)?;
    Ok(Plan {
        supervisor,
        path,
        contents,
        activate,
        deactivate,
    })
}

/// The commands that start and stop an installed unit.
///
/// Deliberately independent of the unit's text: `--disable` and `--unsupervise`
/// must work on a unit this binary did not write, and on a machine where the
/// binary has since moved. Rendering the file to find out how to stop it would
/// make an unrelated failure — a path that acquired a `%`, an executable that
/// was renamed — into "pulse can no longer turn this off".
fn steps(
    supervisor: Supervisor,
    uid: u32,
    label: &str,
    path: &Path,
) -> Result<(Vec<Step>, Vec<Step>)> {
    let unit = supervisor.unit_name(label);
    Ok(match supervisor {
        Supervisor::Systemd => (
            vec![
                Step::required(&["systemctl", "--user", "daemon-reload"]),
                Step::required(&["systemctl", "--user", "enable", "--now", &unit]),
            ],
            vec![Step::required(&[
                "systemctl",
                "--user",
                "disable",
                "--now",
                &unit,
            ])],
        ),
        Supervisor::Launchd => {
            let domain = format!("gui/{uid}");
            let target = format!("{domain}/{label}");
            let plist = path_text(path)?.to_string();
            (
                vec![
                    Step::required(&["launchctl", "enable", &target]),
                    Step::tolerated(&["launchctl", "bootout", &target]),
                    Step::required(&["launchctl", "bootstrap", &domain, &plist]),
                ],
                vec![
                    Step::tolerated(&["launchctl", "bootout", &target]),
                    Step::required(&["launchctl", "disable", &target]),
                ],
            )
        }
    })
}

/// A systemd user unit.
///
/// `Restart=always` with a delay, because the point of the feature is that the
/// sampler comes back. `Type=simple`: the daemon runs in the foreground here and
/// does its own detaching only when `--enable` asks it to, so systemd can track
/// the process it started rather than chasing a fork.
fn systemd_unit(env: &Environment) -> Result<String> {
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str("Description=pulse — agent activity history for herdr\n");
    unit.push_str("Documentation=https://github.com/moneycaringcoder/herdr-pulse\n");
    unit.push_str("After=default.target\n\n");

    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!(
        "ExecStart={}\n",
        systemd_command(&env.exe, &env.args)?
    ));
    unit.push_str(&format!(
        "Environment=\"HERDR_PLUGIN_STATE_DIR={}\"\n",
        systemd_value(&env.state_root)?
    ));
    unit.push_str(&format!(
        "Environment=\"HERDR_SOCKET_PATH={}\"\n",
        systemd_value(&env.socket_path)?
    ));
    unit.push_str(&format!(
        "Environment=\"{}={}\"\n",
        crate::config::SOCKET_IS_DEFAULT_ENV,
        if env.socket_is_default { "1" } else { "0" }
    ));
    unit.push_str("Restart=always\n");
    unit.push_str(&format!("RestartSec={RESTART_SECONDS}\n\n"));

    unit.push_str("[Install]\n");
    unit.push_str("WantedBy=default.target\n");
    Ok(unit)
}

/// `ExecStart` as one line, each argument quoted.
///
/// systemd's own quoting rules, not a shell's: a value is wrapped in double
/// quotes and may not contain one. A path that would need escaping is refused
/// rather than escaped, because a unit file that is subtly wrong fails at boot,
/// months later, in a log nobody is reading.
fn systemd_command(exe: &Path, args: &[String]) -> Result<String> {
    let mut line = format!("\"{}\"", systemd_value(exe)?);
    line.push_str(" \"--daemon\"");
    for arg in args {
        line.push_str(&format!(" \"{}\"", systemd_text(arg)?));
    }
    Ok(line)
}

fn systemd_value(path: &Path) -> Result<String> {
    systemd_text(path_text(path)?)
}

/// Refuses anything systemd would read as something other than itself.
///
/// `%` starts a specifier and `$` a variable reference; both expand into a path
/// that is not the one on disk. A backslash continues a line, a newline ends the
/// directive, and a double quote ends the value. Refused rather than escaped:
/// the failure lands here, in front of the person who typed the command, instead
/// of at the next boot.
fn systemd_text(text: &str) -> Result<String> {
    if text.contains(['"', '\\', '\n', '\r', '%', '$', ';']) {
        return Err(format!(
            "cannot write a unit for `{text}`: it contains a character systemd \
             would reinterpret, and a unit that is subtly wrong fails at boot \
             rather than here"
        )
        .into());
    }
    Ok(text.to_string())
}

/// A launchd user agent.
///
/// `KeepAlive` restarts it, `ThrottleInterval` keeps a failing start from
/// spinning, and `RunAtLoad` starts it at login without waiting for a first
/// failure.
fn launchd_plist(env: &Environment) -> Result<String> {
    let mut arguments = String::new();
    for arg in std::iter::once(path_text(&env.exe)?.to_string())
        .chain(std::iter::once("--daemon".to_string()))
        .chain(env.args.iter().cloned())
    {
        arguments.push_str(&format!("    <string>{}</string>\n", xml(&arg)));
    }
    let variables = format!(
        "    <key>HERDR_PLUGIN_STATE_DIR</key>\n    <string>{}</string>\n\
         \x20   <key>HERDR_SOCKET_PATH</key>\n    <string>{}</string>\n\
         \x20   <key>{}</key>\n    <string>{}</string>\n",
        xml(path_text(&env.state_root)?),
        xml(path_text(&env.socket_path)?),
        crate::config::SOCKET_IS_DEFAULT_ENV,
        if env.socket_is_default { "1" } else { "0" }
    );
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n{arguments}\x20 </array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict>\n{variables}\x20 </dict>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>KeepAlive</key>\n\
         \x20 <true/>\n\
         \x20 <key>ThrottleInterval</key>\n\
         \x20 <integer>{RESTART_SECONDS}</integer>\n\
         </dict>\n\
         </plist>\n",
        xml(&env.label)
    ))
}

/// The five XML entities. A label or a path with an `&` in it is legal on disk
/// and would otherwise produce a plist launchd refuses to parse.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}
/// Where this namespace's unit file would be, whether or not it exists.
pub fn unit_path(paths: &SessionPaths) -> Result<PathBuf> {
    let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
    Ok(unit_dir(supervisor)?.join(supervisor.unit_name(&paths.supervisor_label())))
}

pub fn is_installed(paths: &SessionPaths) -> bool {
    unit_path(paths).is_ok_and(|path| path.exists())
}

/// `pulse --supervise`: atomically replace any current owner with this
/// namespace's platform supervisor.
pub fn install(paths: &SessionPaths, args: &[String]) -> Result<()> {
    let forwarded = crate::daemon::forwarded_args(args)?;
    crate::config::load_with_args(args)?;
    let env = Environment::current(paths, forwarded)?;
    let plan = plan(&env)?;
    let control = crate::daemon::ControlGuard::acquire(paths)?;
    let was_enabled = crate::daemon::is_enabled(paths);

    let had_supervisor = is_installed(paths);
    if had_supervisor {
        for step in &plan.deactivate {
            if let Err(err) = run(step) {
                eprintln!("pulse: {err}");
            }
        }
    }
    if let Err(stop_error) = crate::daemon::stop_sampler_locked(paths, true) {
        if had_supervisor {
            return match restore_start_at_login(paths) {
                Ok(()) => Err(format!(
                    "cannot replace supervision: the sampler could not be stopped \
                     ({stop_error}); start-at-login registration was restored"
                )
                .into()),
                Err(restore_error) => Err(format!(
                    "cannot replace supervision: the sampler could not be stopped \
                     ({stop_error}), and start-at-login registration could not be restored: \
                     {restore_error}"
                )
                .into()),
            };
        }
        return Err(stop_error);
    }

    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plan.path, &plan.contents)?;
    crate::daemon::mark_enabled(paths, true)?;
    for step in &plan.activate {
        if let Err(err) = run(step) {
            let _ = std::fs::remove_file(&plan.path);
            let _ = crate::daemon::mark_enabled(paths, was_enabled);
            return Err(err);
        }
    }
    drop(control);

    if let Err(err) = crate::daemon::await_started(paths, std::time::Duration::from_secs(5)) {
        let _control = crate::daemon::ControlGuard::acquire(paths)?;
        for step in &plan.deactivate {
            let _ = run(step);
        }
        let _ = std::fs::remove_file(&plan.path);
        crate::daemon::mark_enabled(paths, was_enabled)?;
        return Err(err);
    }

    println!("pulse: supervision installed at {}", plan.path.display());
    println!(
        "pulse: the sampler now starts at login and restarts within {RESTART_SECONDS}s if it \
         stops. Every bucket it was not running for stays a gap, which is what a gap is for."
    );
    println!("pulse: remove it with `pulse --unsupervise`.");
    Ok(())
}

/// `pulse --unsupervise`: stop and remove only this namespace's unit.
pub fn remove(paths: &SessionPaths) -> Result<()> {
    let _control = crate::daemon::ControlGuard::acquire(paths)?;
    let path = unit_path(paths)?;
    if !path.exists() {
        println!("pulse: no supervision installed; nothing to remove.");
        return Ok(());
    }
    crate::daemon::mark_enabled(paths, false)?;
    for step in &deactivation(paths)? {
        if let Err(err) = run(step) {
            eprintln!("pulse: {err}");
        }
    }
    crate::daemon::stop_sampler_locked(paths, false)?;
    std::fs::remove_file(&path)?;
    println!("pulse: supervision removed from {}", path.display());
    println!(
        "pulse: recorded history is untouched; `pulse --enable` starts an unsupervised sampler."
    );
    Ok(())
}

/// Starts this namespace's installed unit. Caller serializes lifecycle state.
pub fn start(paths: &SessionPaths) -> Result<()> {
    let (activate, _) = lifecycle_steps(paths)?;
    for step in &activate {
        run(step)?;
    }
    Ok(())
}

/// Stops this namespace's installed unit. Caller serializes lifecycle state.
pub fn stop(paths: &SessionPaths) -> Result<()> {
    for step in &deactivation(paths)? {
        run(step)?;
    }
    Ok(())
}

pub fn restore_start_at_login(paths: &SessionPaths) -> Result<()> {
    let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
    run(&start_at_login_step(
        supervisor,
        uid(),
        &paths.supervisor_label(),
    ))
}

fn start_at_login_step(supervisor: Supervisor, uid: u32, label: &str) -> Step {
    match supervisor {
        Supervisor::Systemd => {
            let unit = supervisor.unit_name(label);
            Step::required(&["systemctl", "--user", "enable", &unit])
        }
        Supervisor::Launchd => {
            let target = format!("gui/{uid}/{label}");
            Step::required(&["launchctl", "enable", &target])
        }
    }
}

fn deactivation(paths: &SessionPaths) -> Result<Vec<Step>> {
    Ok(lifecycle_steps(paths)?.1)
}

fn lifecycle_steps(paths: &SessionPaths) -> Result<(Vec<Step>, Vec<Step>)> {
    let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
    steps(
        supervisor,
        uid(),
        &paths.supervisor_label(),
        &unit_path(paths)?,
    )
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| format!("cannot supervise a non-UTF-8 path: {}", path.display()).into())
}

fn run(step: &Step) -> Result<()> {
    match capture(step) {
        Ok(_) => Ok(()),
        Err(err) if step.tolerated => {
            // Reported, never fatal. "Already stopped" is the state the caller
            // wanted, and silence here would hide a supervisor that is missing
            // entirely.
            eprintln!("pulse: {err}");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Runs one supervisor command and returns its stdout.
///
/// The single place this crate's supervision spawns anything, so the
/// source-level audit in `tests/read_only.rs` has exactly one site to sanction.
/// A non-zero exit is an error carrying the supervisor's own stderr: "unit does
/// not exist" from systemd says more than anything this module could invent.
fn capture(step: &Step) -> Result<String> {
    let (program, args) = step
        .argv
        .split_first()
        .ok_or("empty supervision command, which is a bug in pulse")?;
    let mut spawn = Command::new(program);
    spawn.args(args);
    let output = spawn.output().map_err(|err| {
        format!(
            "`{}` could not be run: {err}. Supervision needs it on PATH.",
            step.argv.join(" ")
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`{}` failed: {}", step.argv.join(" "), stderr.trim()).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_registration_never_starts_a_second_sampler() {
        assert_eq!(
            start_at_login_step(Supervisor::Systemd, 501, LABEL).argv,
            [
                "systemctl",
                "--user",
                "enable",
                "dev.herdr.pulse.sampler.service"
            ]
        );
        assert_eq!(
            start_at_login_step(Supervisor::Launchd, 501, LABEL).argv,
            ["launchctl", "enable", "gui/501/dev.herdr.pulse.sampler"]
        );
    }
}
