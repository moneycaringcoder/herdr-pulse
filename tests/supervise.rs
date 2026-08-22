//! Supervision tests: the unit text and the commands, without a supervisor.
//!
//! Nothing here installs anything. `supervise::plan` is pure — it takes an
//! [`Environment`] and returns the file that would be written and the commands
//! that would run — so a unit for either platform can be pinned exactly without
//! a systemd, a launchd, or a real `$HOME` anywhere near the test.
//!
//! What these defend is narrow and load-bearing: the unit must carry the state
//! directory and socket path resolved at install time, because a supervisor
//! starts the sampler with neither variable set; a path the platform would
//! reinterpret must be refused rather than escaped; and supervision must leave
//! recording untouched, gaps included.

use std::path::PathBuf;

use pulse::supervise::{self, Environment, Supervisor, LABEL, RESTART_SECONDS};

fn env(args: &[&str]) -> Environment {
    Environment {
        exe: PathBuf::from("/home/dev/.local/bin/pulse"),
        state_dir: PathBuf::from("/home/dev/.local/state/herdr/plugins/pulse"),
        socket_path: Some(PathBuf::from("/home/dev/.config/herdr/herdr.sock")),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        unit_dir: PathBuf::from("/home/dev/.config/systemd/user"),
        uid: 1000,
    }
}

#[test]
fn the_unit_carries_the_paths_resolved_at_install_time() {
    // A supervisor spawns the sampler with none of herdr's environment. If the
    // unit did not name the state directory, the supervised sampler would
    // re-derive one, and the day a variable changed underneath it, it would
    // record into a directory the panes never read — a plugin that looks like it
    // is working and is not.
    let plan = supervise::plan(&env(&[])).expect("plan");

    assert!(
        plan.contents
            .contains("/home/dev/.local/state/herdr/plugins/pulse"),
        "the unit names the state directory: {}",
        plan.contents
    );
    assert!(
        plan.contents.contains("/home/dev/.config/herdr/herdr.sock"),
        "and the socket it was installed against: {}",
        plan.contents
    );
    assert!(
        plan.contents.contains("--daemon"),
        "and runs the sampler in the foreground: {}",
        plan.contents
    );
}

#[test]
fn recording_arguments_are_baked_in_and_reading_ones_are_not() {
    // The unit is the only record of what the user asked for: there is no
    // command line behind it. Filtered by the same `forwarded_args` the detached
    // child already goes through, so a window like `--since` — which changes
    // nothing about recording — cannot end up frozen into a unit that outlives
    // the afternoon somebody typed it.
    let typed = [
        "--supervise",
        "--interval",
        "10",
        "--agents",
        "--since",
        "30m",
    ]
    .map(str::to_string)
    .to_vec();
    let forwarded = pulse::daemon::forwarded_args(&typed).expect("forward");
    let mut environment = env(&[]);
    environment.args = forwarded;

    let plan = supervise::plan(&environment).expect("plan");

    assert!(plan.contents.contains("--interval"), "{}", plan.contents);
    assert!(plan.contents.contains("10"), "{}", plan.contents);
    assert!(plan.contents.contains("--agents"), "{}", plan.contents);
    assert!(
        !plan.contents.contains("--since") && !plan.contents.contains("30m"),
        "a reading window is not a recording setting: {}",
        plan.contents
    );
}

#[test]
fn a_restart_delay_is_stated_rather_than_left_to_the_supervisor() {
    // Without one, a sampler that fails on every start spins as fast as the
    // supervisor will let it. With one, the hole it leaves is a bucket wide at
    // the default width.
    let plan = supervise::plan(&env(&[])).expect("plan");
    let stated = plan.contents.contains(&RESTART_SECONDS.to_string());
    assert!(
        stated,
        "the unit states its restart delay: {}",
        plan.contents
    );

    match plan.supervisor {
        Supervisor::Systemd => {
            assert!(
                plan.contents.contains("Restart=always"),
                "{}",
                plan.contents
            );
            assert!(
                plan.contents.contains("WantedBy=default.target"),
                "and starts at login: {}",
                plan.contents
            );
        }
        Supervisor::Launchd => {
            assert!(
                plan.contents.contains("<key>KeepAlive</key>"),
                "{}",
                plan.contents
            );
            assert!(
                plan.contents.contains("<key>RunAtLoad</key>"),
                "and starts at login: {}",
                plan.contents
            );
        }
    }
}

#[test]
fn the_commands_activate_and_deactivate_the_same_unit() {
    // A deactivate that named something else would leave the sampler running
    // after `--disable`, which is the one thing that verb promises.
    let plan = supervise::plan(&env(&[])).expect("plan");
    let flat = |commands: &[Vec<String>]| commands.concat().join(" ");

    let activate = flat(&plan.activate);
    let deactivate = flat(&plan.deactivate);
    assert!(activate.contains(LABEL), "{activate}");
    assert!(deactivate.contains(LABEL), "{deactivate}");

    match plan.supervisor {
        Supervisor::Systemd => {
            assert!(
                activate.contains("systemctl --user enable --now"),
                "{activate}"
            );
            assert!(
                deactivate.contains("systemctl --user disable --now"),
                "a stop that left the unit enabled would come back at boot: {deactivate}"
            );
        }
        Supervisor::Launchd => {
            assert!(
                activate.contains("launchctl bootstrap gui/1000"),
                "{activate}"
            );
            assert!(
                deactivate.contains("launchctl bootout gui/1000"),
                "{deactivate}"
            );
        }
    }
}

#[test]
fn the_unit_file_lands_where_the_platform_looks_for_it() {
    let plan = supervise::plan(&env(&[])).expect("plan");
    let name = plan
        .path
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .to_string();

    match plan.supervisor {
        Supervisor::Systemd => {
            assert_eq!(name, format!("{LABEL}.service"));
            assert!(plan.path.starts_with("/home/dev/.config/systemd/user"));
        }
        Supervisor::Launchd => assert_eq!(name, format!("{LABEL}.plist")),
    }
}

#[test]
fn a_path_the_platform_would_reinterpret_is_refused_rather_than_escaped() {
    // Not hypothetical: `%i` is a systemd specifier and `$HOME` is a variable
    // reference, and either one silently expands into a path that is not the one
    // on disk. A unit that is subtly wrong fails at boot, months later, in a log
    // nobody is reading — so the refusal happens here, in front of the person
    // who typed the command.
    for hostile in ["/home/de%v/pulse", "/home/$USER/pulse", "/home/d\"ev/pulse"] {
        let mut broken = env(&[]);
        broken.exe = PathBuf::from(hostile);
        let refused = supervise::plan(&broken);
        if matches!(Supervisor::current(), Some(Supervisor::Systemd)) {
            assert!(refused.is_err(), "{hostile} should be refused");
        }
    }
}

#[test]
fn a_plan_writes_nothing_and_runs_nothing() {
    // The whole reason `plan` is separate: these tests must be able to pin every
    // byte of a real unit without a supervisor on the machine and without
    // touching a real home directory.
    let before = PathBuf::from("/home/dev/.config/systemd/user");
    let plan = supervise::plan(&env(&[])).expect("plan");

    assert!(
        !before.exists(),
        "the test's fictional home stays fictional"
    );
    assert!(!plan.path.exists(), "and no unit was written");
}

#[test]
fn nothing_in_the_unit_claims_the_sampler_was_watching_while_it_was_not() {
    // The one thing a reader might assume wrongly. A supervisor restarting the
    // sampler is not the sampler having observed the meantime, and there is no
    // knob here that could make it look that way: the unit passes recording
    // arguments and paths, and nothing that touches how a gap is judged.
    let plan = supervise::plan(&env(&["--interval", "10"])).expect("plan");

    for forbidden in ["backfill", "catch-up", "catchup", "fill", "assume"] {
        assert!(
            !plan.contents.to_lowercase().contains(forbidden),
            "the unit must not carry anything that fills in unobserved time: {}",
            plan.contents
        );
    }
}
