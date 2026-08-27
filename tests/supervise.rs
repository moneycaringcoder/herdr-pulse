//! Supervision tests: the unit text and the commands, without a supervisor.
//!
//! Nothing here installs anything. `supervise::plan_for` is pure — it takes a
//! supervisor and an [`Environment`] and returns the file that would be written
//! and the steps that would run — so both platforms' units are pinned on every
//! host. That matters: the launchd half of an earlier version was unreachable on
//! Linux, and a `bootout` that does not survive a reboot got as far as review
//! because no test on the machine doing the reviewing could see it.
//!
//! What these defend is narrow and load-bearing. The unit must carry the state
//! directory and socket path resolved at install time, because a supervisor
//! starts the sampler with neither variable set. A path the platform would
//! reinterpret must be refused rather than escaped. Stopping must be durable on
//! both platforms, or `--disable` is a lie by morning. And supervision must
//! leave recording untouched, gaps included.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use pulse::supervise::{self, Environment, Step, Supervisor, LABEL, RESTART_SECONDS};

const BOTH: [Supervisor; 2] = [Supervisor::Systemd, Supervisor::Launchd];

fn env(args: &[&str]) -> Environment {
    Environment {
        exe: PathBuf::from("/home/dev/.local/bin/pulse"),
        state_root: PathBuf::from("/home/dev/.local/state/herdr/plugins/pulse"),
        socket_path: PathBuf::from("/home/dev/.config/herdr/herdr.sock"),
        socket_is_default: true,
        label: LABEL.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        unit_dir: PathBuf::from("/home/dev/.config/systemd/user"),
        uid: 1000,
    }
}

fn flat(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|step| step.argv.join(" "))
        .collect::<Vec<_>>()
        .join(" ; ")
}

#[test]
fn every_unit_carries_the_paths_resolved_at_install_time() {
    // A supervisor spawns the sampler with none of herdr's environment. If the
    // unit did not name the state directory, the supervised sampler would
    // re-derive one, and the day a variable changed underneath it, it would
    // record into a directory the panes never read — a plugin that looks like it
    // is working and is not.
    for supervisor in BOTH {
        let plan = supervise::plan_for(supervisor, &env(&[])).expect("plan");

        assert!(
            plan.contents
                .contains("/home/dev/.local/state/herdr/plugins/pulse"),
            "{supervisor:?} names the state directory: {}",
            plan.contents
        );
        assert!(
            plan.contents.contains("/home/dev/.config/herdr/herdr.sock"),
            "{supervisor:?} names the socket it was installed against: {}",
            plan.contents
        );
        assert!(
            plan.contents.contains("PULSE_SOCKET_IS_DEFAULT")
                && (plan.contents.contains("=1") || plan.contents.contains("<string>1</string>")),
            "{supervisor:?} preserves default-state compatibility: {}",
            plan.contents
        );
        assert!(
            plan.contents.contains("--daemon"),
            "{supervisor:?} runs the sampler in the foreground: {}",
            plan.contents
        );
        assert!(
            plan.contents.contains("/home/dev/.local/bin/pulse"),
            "{supervisor:?} names this binary: {}",
            plan.contents
        );
    }
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

    for supervisor in BOTH {
        let plan = supervise::plan_for(supervisor, &environment).expect("plan");

        assert!(plan.contents.contains("--interval"), "{}", plan.contents);
        assert!(plan.contents.contains("10"), "{}", plan.contents);
        assert!(plan.contents.contains("--agents"), "{}", plan.contents);
        assert!(
            !plan.contents.contains("--since") && !plan.contents.contains("30m"),
            "a reading window is not a recording setting: {}",
            plan.contents
        );
    }
}

#[test]
fn both_units_start_at_login_and_restart_after_a_stated_delay() {
    // Without a delay, a sampler that fails on every start spins as fast as the
    // supervisor will let it. Without start-at-login, supervision buys nothing
    // that `--enable` did not already.
    let systemd = supervise::plan_for(Supervisor::Systemd, &env(&[])).expect("plan");
    assert!(systemd.contents.contains("Restart=always"));
    assert!(systemd
        .contents
        .contains(&format!("RestartSec={RESTART_SECONDS}")));
    assert!(systemd.contents.contains("WantedBy=default.target"));

    let launchd = supervise::plan_for(Supervisor::Launchd, &env(&[])).expect("plan");
    assert!(launchd.contents.contains("<key>KeepAlive</key>"));
    assert!(launchd.contents.contains("<key>RunAtLoad</key>"));
    assert!(launchd.contents.contains(&format!(
        "<key>ThrottleInterval</key>\n  <integer>{RESTART_SECONDS}</integer>"
    )));
}

#[test]
fn stopping_is_durable_on_both_platforms() {
    // The half that is easy to get wrong. systemd's `disable` takes the unit out
    // of the boot sequence; launchd's `bootout` only unloads it for this session
    // and the plist stays where launchd looks at the next login, so without
    // `launchctl disable` the sampler is back by morning and `--disable` was a
    // lie.
    let systemd = supervise::plan_for(Supervisor::Systemd, &env(&[])).expect("plan");
    let stop = flat(&systemd.deactivate);
    assert!(
        stop.contains("systemctl --user disable --now"),
        "a stop that left the unit enabled would come back at boot: {stop}"
    );

    let launchd = supervise::plan_for(Supervisor::Launchd, &env(&[])).expect("plan");
    let stop = flat(&launchd.deactivate);
    assert!(
        stop.contains(&format!("launchctl bootout gui/1000/{LABEL}")),
        "{stop}"
    );
    assert!(
        stop.contains(&format!("launchctl disable gui/1000/{LABEL}")),
        "bootout alone is undone by the next login: {stop}"
    );
}

#[test]
fn starting_clears_whatever_an_earlier_install_left_behind() {
    // `--supervise` twice, or `--enable` after a `--disable`, must mean the same
    // as once. launchd refuses to bootstrap a label that is already loaded, and
    // refuses to start one its disabled database still names.
    let launchd = supervise::plan_for(Supervisor::Launchd, &env(&[])).expect("plan");
    let start = flat(&launchd.activate);

    assert!(
        start.contains(&format!("launchctl enable gui/1000/{LABEL}")),
        "a label disabled by an earlier --disable would never start: {start}"
    );
    assert!(
        start.contains("launchctl bootout") && start.contains("launchctl bootstrap"),
        "a stale registration is cleared before bootstrapping: {start}"
    );
    let bootout = launchd
        .activate
        .iter()
        .find(|step| step.argv.contains(&"bootout".to_string()))
        .expect("a bootout step");
    assert!(
        bootout.tolerated,
        "nothing loaded is the state we wanted, not a failure"
    );

    let systemd = supervise::plan_for(Supervisor::Systemd, &env(&[])).expect("plan");
    let start = flat(&systemd.activate);
    assert!(start.contains("systemctl --user daemon-reload"), "{start}");
    assert!(start.contains("systemctl --user enable --now"), "{start}");
}

#[test]
fn the_unit_file_lands_where_the_platform_looks_for_it() {
    for supervisor in BOTH {
        let plan = supervise::plan_for(supervisor, &env(&[])).expect("plan");
        let name = plan
            .path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .to_string();
        match supervisor {
            Supervisor::Systemd => assert_eq!(name, format!("{LABEL}.service")),
            Supervisor::Launchd => assert_eq!(name, format!("{LABEL}.plist")),
        }
    }
}

#[test]
fn named_sessions_get_distinct_labels_plans_and_root_environment() {
    let mut a = env(&[]);
    a.socket_path = PathBuf::from("/tmp/herdr-a.sock");
    a.socket_is_default = false;
    a.label = format!("{LABEL}.socket-2f746d702f68657264722d612e736f636b");
    let mut b = env(&[]);
    b.socket_path = PathBuf::from("/tmp/herdr-b.sock");
    b.socket_is_default = false;
    b.label = format!("{LABEL}.socket-2f746d702f68657264722d622e736f636b");

    for supervisor in BOTH {
        let a_plan = supervise::plan_for(supervisor, &a).expect("A plan");
        let b_plan = supervise::plan_for(supervisor, &b).expect("B plan");
        assert_ne!(a_plan.path, b_plan.path);
        let a_identity = format!("{}\n{}", a_plan.contents, flat(&a_plan.activate));
        let b_identity = format!("{}\n{}", b_plan.contents, flat(&b_plan.activate));
        assert!(a_identity.contains(&a.label), "{a_identity}");
        assert!(b_identity.contains(&b.label), "{b_identity}");
        assert!(a_plan.contents.contains("=0") || a_plan.contents.contains("<string>0</string>"));
        assert!(b_plan.contents.contains("=0") || b_plan.contents.contains("<string>0</string>"));
        assert!(a_plan.contents.contains("/tmp/herdr-a.sock"));
        assert!(b_plan.contents.contains("/tmp/herdr-b.sock"));
        assert!(
            a_plan
                .contents
                .contains("/home/dev/.local/state/herdr/plugins/pulse"),
            "unit exports the state root, not a sessions child"
        );
    }
}

#[test]
fn a_path_systemd_would_reinterpret_is_refused_rather_than_escaped() {
    // Not hypothetical: `%i` is a specifier and `$HOME` a variable reference, and
    // either one silently expands into a path that is not the one on disk. A unit
    // that is subtly wrong fails at boot, months later, in a log nobody is
    // reading — so the refusal happens in front of the person who typed the
    // command.
    for hostile in [
        "/home/de%v/pulse",
        "/home/$USER/pulse",
        "/home/d\"ev/pulse",
        "/home/dev\\/pulse",
    ] {
        let mut broken = env(&[]);
        broken.exe = PathBuf::from(hostile);
        assert!(
            supervise::plan_for(Supervisor::Systemd, &broken).is_err(),
            "{hostile} should be refused"
        );
    }
}

#[test]
fn a_path_launchd_would_reinterpret_is_escaped_rather_than_refused() {
    // XML has an answer that systemd's directive syntax does not: an ampersand
    // in a path is a legal file name and a well-defined entity, so it is written
    // out rather than turned into an error the user cannot act on.
    let mut awkward = env(&[]);
    awkward.exe = PathBuf::from("/home/dev/tools & toys/pulse");

    let plan = supervise::plan_for(Supervisor::Launchd, &awkward).expect("plan");

    assert!(
        plan.contents.contains("tools &amp; toys"),
        "{}",
        plan.contents
    );
    assert!(
        !plan.contents.contains("tools & toys"),
        "a bare ampersand is a plist launchd refuses to parse: {}",
        plan.contents
    );
}

#[test]
fn non_utf8_paths_are_refused_instead_of_redirected_lossily() {
    let mut broken = env(&[]);
    broken.socket_path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff, b's']));

    for supervisor in BOTH {
        let err = supervise::plan_for(supervisor, &broken)
            .expect_err("a lossy path would target a different socket");
        assert!(err.to_string().contains("non-UTF-8"), "{err}");
    }
}

#[test]
fn a_plan_writes_nothing_and_runs_nothing() {
    // The whole reason `plan_for` is separate: these tests must be able to pin
    // every byte of a real unit without a supervisor on the machine and without
    // touching a real home directory.
    for supervisor in BOTH {
        let plan = supervise::plan_for(supervisor, &env(&[])).expect("plan");
        assert!(!plan.path.exists(), "no unit was written: {:?}", plan.path);
    }
    assert!(
        !PathBuf::from("/home/dev/.config/systemd/user").exists(),
        "the test's fictional home stays fictional"
    );
}

#[test]
fn nothing_in_a_unit_claims_the_sampler_was_watching_while_it_was_not() {
    // The one thing a reader might assume wrongly. A supervisor restarting the
    // sampler is not the sampler having observed the meantime, and there is no
    // knob here that could make it look that way: a unit passes recording
    // arguments and paths, and nothing that touches how a gap is judged.
    for supervisor in BOTH {
        let plan = supervise::plan_for(supervisor, &env(&["--interval", "10"])).expect("plan");
        for forbidden in ["backfill", "catch-up", "catchup", "assume"] {
            assert!(
                !plan.contents.to_lowercase().contains(forbidden),
                "{supervisor:?} must not carry anything that fills in unobserved time: {}",
                plan.contents
            );
        }
    }
}
