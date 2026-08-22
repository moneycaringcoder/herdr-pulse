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
//! the same buckets to the same file. In particular a restarted unit leaves a
//! gap for the time it was not running, exactly as a hand-started sampler does:
//! **continuity of the unit is not continuity of observation**, and a supervisor
//! that quietly papered over its own downtime would be drawing zeros for minutes
//! nobody watched.
//!
//! # Why the environment is baked into the unit
//!
//! herdr injects `HERDR_PLUGIN_STATE_DIR` and `HERDR_SOCKET_PATH` into the
//! commands it spawns. A supervisor spawns the sampler with neither, so the unit
//! carries the paths this machine resolves *now*, written out in full. Letting
//! the supervised sampler re-derive them would work until the day a variable
//! changes underneath it, and then it would record history into a directory the
//! panes never read — a plugin that looks like it is working and is not.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::non_empty_env;
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

/// Which supervisor this build targets.
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

    fn unit_name(self) -> String {
        match self {
            Self::Systemd => format!("{LABEL}.service"),
            Self::Launchd => format!("{LABEL}.plist"),
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
    /// Commands that install and start it, in order.
    pub activate: Vec<Vec<String>>,
    /// Commands that stop it and take it out of the boot sequence, in order.
    /// The file itself is removed separately, so a stop can be undone.
    pub deactivate: Vec<Vec<String>>,
}

/// What the supervised sampler needs handed to it, resolved on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub exe: PathBuf,
    pub state_dir: PathBuf,
    pub socket_path: Option<PathBuf>,
    /// Recording arguments to bake in, already filtered by
    /// [`crate::daemon::forwarded_args`].
    pub args: Vec<String>,
    /// Directory the unit file goes in.
    pub unit_dir: PathBuf,
    /// The user's numeric id, which launchd needs to name a session.
    pub uid: u32,
}

impl Environment {
    /// This machine, now.
    pub fn current(args: Vec<String>) -> Result<Self> {
        let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
        Ok(Self {
            exe: std::env::current_exe()?,
            state_dir: crate::config::state_dir(),
            socket_path: crate::herdr::socket_path().ok(),
            args,
            unit_dir: unit_dir(supervisor),
            uid: uid(),
        })
    }
}

const UNSUPPORTED: &str =
    "supervision is available on Linux (systemd) and macOS (launchd) only; on \
     other systems `pulse --enable` still runs the sampler until the machine stops";

/// Where a unit file lives, per platform convention.
///
/// The systemd half asks the running user manager rather than deriving a path,
/// and this is not fussiness. A user manager is started by logind long before a
/// shell exists, so it does not inherit an `XDG_CONFIG_HOME` set in a shell rc.
/// Deriving `$XDG_CONFIG_HOME/systemd/user` from *our* environment therefore
/// writes the unit somewhere the manager never reads, and `enable` fails with
/// "unit does not exist" while a perfectly good file sits on disk. Observed on
/// the machine this was written on.
fn unit_dir(supervisor: Supervisor) -> PathBuf {
    match supervisor {
        Supervisor::Systemd => systemd_unit_dir(),
        Supervisor::Launchd => home().join("Library").join("LaunchAgents"),
    }
}

/// The manager's own per-user unit directory, from `systemctl --user show`.
///
/// `UnitPath` lists every directory the manager searches, most specific first.
/// The one wanted is the writable per-user config directory — `user.control` and
/// the generator directories are systemd's own scratch space and a hand-written
/// unit does not belong in them. Falls back to the documented default when there
/// is no manager to ask, which is also the case where enabling would fail
/// anyway, and where the error is worth more than a guess.
fn systemd_unit_dir() -> PathBuf {
    let default = home().join(".config").join("systemd").join("user");
    let Ok(shown) = capture(&argv(&[
        "systemctl",
        "--user",
        "show",
        "--property=UnitPath",
    ])) else {
        return default;
    };
    let home = home();
    shown
        .trim()
        .strip_prefix("UnitPath=")
        .unwrap_or_default()
        .split_whitespace()
        .map(PathBuf::from)
        .find(|path| path.starts_with(&home) && path.ends_with("systemd/user"))
        .unwrap_or(default)
}

fn home() -> PathBuf {
    non_empty_env("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn uid() -> u32 {
    // SAFETY: `getuid` takes no arguments, touches no memory, and cannot fail.
    unsafe { libc_getuid() }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// The plan for an environment. Pure: it writes nothing and runs nothing.
pub fn plan(env: &Environment) -> Result<Plan> {
    let supervisor = Supervisor::current().ok_or(UNSUPPORTED)?;
    let path = env.unit_dir.join(supervisor.unit_name());
    let contents = match supervisor {
        Supervisor::Systemd => systemd_unit(env)?,
        Supervisor::Launchd => launchd_plist(env)?,
    };
    let unit = supervisor.unit_name();
    let (activate, deactivate) = match supervisor {
        Supervisor::Systemd => (
            vec![
                argv(&["systemctl", "--user", "daemon-reload"]),
                argv(&["systemctl", "--user", "enable", "--now", &unit]),
            ],
            vec![argv(&["systemctl", "--user", "disable", "--now", &unit])],
        ),
        Supervisor::Launchd => {
            let domain = format!("gui/{}", env.uid);
            let target = format!("{domain}/{LABEL}");
            (
                vec![argv(&[
                    "launchctl",
                    "bootstrap",
                    &domain,
                    &path.to_string_lossy(),
                ])],
                vec![argv(&["launchctl", "bootout", &target])],
            )
        }
    };
    Ok(Plan {
        supervisor,
        path,
        contents,
        activate,
        deactivate,
    })
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
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
        systemd_value(&env.state_dir)?
    ));
    if let Some(socket) = &env.socket_path {
        unit.push_str(&format!(
            "Environment=\"HERDR_SOCKET_PATH={}\"\n",
            systemd_value(socket)?
        ));
    }
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
    systemd_text(&path.to_string_lossy())
}

fn systemd_text(text: &str) -> Result<String> {
    if text.contains(['"', '\\', '\n', '%', '$']) {
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
    for arg in std::iter::once(env.exe.to_string_lossy().to_string())
        .chain(std::iter::once("--daemon".to_string()))
        .chain(env.args.iter().cloned())
    {
        arguments.push_str(&format!("    <string>{}</string>\n", xml(&arg)));
    }
    let mut variables = format!(
        "    <key>HERDR_PLUGIN_STATE_DIR</key>\n    <string>{}</string>\n",
        xml(&env.state_dir.to_string_lossy())
    );
    if let Some(socket) = &env.socket_path {
        variables.push_str(&format!(
            "    <key>HERDR_SOCKET_PATH</key>\n    <string>{}</string>\n",
            xml(&socket.to_string_lossy())
        ));
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LABEL}</string>\n\
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
         </plist>\n"
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

/// Where the unit file would be, whether or not it exists.
pub fn unit_path() -> Option<PathBuf> {
    let supervisor = Supervisor::current()?;
    Some(unit_dir(supervisor).join(supervisor.unit_name()))
}

/// Whether a unit file this plugin wrote is on disk.
pub fn is_installed() -> bool {
    unit_path().is_some_and(|path| path.exists())
}

/// `pulse --supervise`: write the unit and start it.
///
/// The sampler the user may already have running is stopped first. Two samplers
/// writing one history file would each rewrite what the other just wrote, and
/// the supervisor's copy is the one that survives a reboot.
pub fn install(args: &[String]) -> Result<()> {
    let forwarded = crate::daemon::forwarded_args(args)?;
    crate::config::load_with_args(args)?;
    let env = Environment::current(forwarded)?;
    let plan = plan(&env)?;

    if crate::daemon::live_pid().is_some() {
        crate::daemon::disable()?;
    }

    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plan.path, &plan.contents)?;
    for command in &plan.activate {
        if let Err(err) = run(command) {
            // A file on disk that no supervisor loaded is worse than no file at
            // all: `is_installed` would be true, `--enable` would route to a unit
            // nothing runs, and the sampler would never start again. Undo the
            // write and hand back the supervisor's own words.
            let _ = std::fs::remove_file(&plan.path);
            return Err(err);
        }
    }
    crate::daemon::mark_enabled(true);

    println!("pulse: supervision installed at {}", plan.path.display());
    println!(
        "pulse: the sampler now starts at login and restarts within {RESTART_SECONDS}s if it \
         stops. A restart leaves a gap for the time it was not running, which is what a gap \
         is for."
    );
    println!("pulse: remove it with `pulse --unsupervise`.");
    Ok(())
}

/// `pulse --unsupervise`: stop the unit and delete it.
///
/// The sampler stops with it. Anything already recorded stays recorded: this
/// removes a supervisor, not a history.
pub fn remove() -> Result<()> {
    let Some(path) = unit_path() else {
        return Err(UNSUPPORTED.into());
    };
    if !path.exists() {
        println!("pulse: no supervision installed; nothing to remove.");
        return Ok(());
    }
    let env = Environment::current(Vec::new())?;
    for command in &plan(&env)?.deactivate {
        // Best effort: a unit that was already stopped, or a supervisor that has
        // forgotten it, must not stop the file from being removed. Leaving the
        // file behind would leave `--enable` routing to a unit nothing runs.
        let _ = run(command);
    }
    std::fs::remove_file(&path)?;
    crate::daemon::mark_enabled(false);
    println!("pulse: supervision removed from {}", path.display());
    println!(
        "pulse: recorded history is untouched; `pulse --enable` starts an unsupervised sampler."
    );
    Ok(())
}

/// Starts the installed unit. Used by `--enable` when supervision is installed,
/// so one verb means one thing whether or not a supervisor owns the sampler.
pub fn start() -> Result<()> {
    let env = Environment::current(Vec::new())?;
    for command in &plan(&env)?.activate {
        run(command)?;
    }
    Ok(())
}

/// Stops the installed unit and takes it out of the boot sequence, leaving the
/// file in place.
///
/// `--disable` has to reach this, or it would be a lie: the supervisor would
/// restart the sampler seconds later, and on the next boot regardless.
pub fn stop() -> Result<()> {
    let env = Environment::current(Vec::new())?;
    for command in &plan(&env)?.deactivate {
        run(command)?;
    }
    Ok(())
}

fn run(command: &[String]) -> Result<()> {
    capture(command).map(|_| ())
}

/// Runs one supervisor command and returns its stdout.
///
/// The single place this crate's supervision spawns anything, so the
/// source-level audit in `tests/read_only.rs` has exactly one site to sanction.
/// A non-zero exit is an error carrying the supervisor's own stderr: "unit does
/// not exist" from systemd says more than anything this module could invent.
fn capture(command: &[String]) -> Result<String> {
    let (program, args) = command
        .split_first()
        .ok_or("empty supervision command, which is a bug in pulse")?;
    let mut spawn = Command::new(program);
    spawn.args(args);
    let output = spawn.output().map_err(|err| {
        format!(
            "`{}` could not be run: {err}. Supervision needs it on PATH.",
            command.join(" ")
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`{}` failed: {}", command.join(" "), stderr.trim()).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
