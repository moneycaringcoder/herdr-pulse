//! Proof that this plugin never writes to a user's repository, never shells out
//! to anything it has not declared, and cannot reach the network.
//!
//! These are the non-negotiable safety properties. Unlike a plugin that inspects
//! git, pulse reads nothing but the herdr socket — so the guarantee here is
//! stronger and simpler than "every git call is read-only": **there are no git
//! calls at all**, and nothing outside the plugin's own state directory is ever
//! written.
//!
//! A property nobody can observe failing is a property that will eventually
//! fail, so each of these is checked mechanically rather than asserted in prose.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every file under `root`, with its length and contents hash, so any creation,
/// deletion or modification anywhere in the tree shows up as a difference.
///
/// Contents rather than mtime: a write that happens to preserve a timestamp is
/// exactly the kind of thing this test exists to catch.
fn fingerprint(root: &Path) -> BTreeMap<PathBuf, (u64, u64)> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, (u64, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&path, root, out);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.insert(relative, (meta.len(), hash(&bytes)));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// FNV-1a. A cryptographic hash would be a dependency; this only has to notice
/// that bytes changed.
fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary() -> PathBuf {
    // `CARGO_BIN_EXE_<name>` is set by cargo for integration tests and points at
    // the binary this very build produced, so the test can never drift onto a
    // stale artefact from an earlier build.
    PathBuf::from(env!("CARGO_BIN_EXE_pulse"))
}

/// A throwaway directory, removed on drop even when a test panics.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pulse-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Builds a small real git repository to run the plugin against.
///
/// Returns `None` when git is unavailable, so the suite still passes on a
/// machine without it rather than failing for an unrelated reason.
fn fixture_repo(dir: &Path) -> Option<PathBuf> {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).ok()?;
    let git = |args: &[&str]| -> Option<()> {
        let status = Command::new("git")
            // Git's own background maintenance writes inside `.git` on its own
            // schedule — `.git/objects/maintenance.lock` appears and disappears
            // without anyone asking. The fingerprint below cannot tell that
            // apart from the plugin writing, so a run that happened to straddle
            // a maintenance tick failed a test about the plugin's behaviour for
            // a reason that had nothing to do with the plugin. Turning both off
            // makes the fixture quiet, so a difference means what the test says
            // it means.
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(())
    };
    git(&["init", "--quiet"])?;
    std::fs::write(repo.join("tracked.txt"), "tracked\n").ok()?;
    git(&["add", "."])?;
    git(&["commit", "--quiet", "-m", "initial"])?;
    // An uncommitted change and an untracked file, so the fingerprint covers a
    // dirty tree and not only a pristine one.
    std::fs::write(repo.join("tracked.txt"), "tracked, modified\n").ok()?;
    std::fs::write(repo.join("untracked.txt"), "untracked\n").ok()?;
    Some(repo)
}

/// Runs the plugin binary with its state directory pointed somewhere harmless.
fn run_plugin(args: &[&str], cwd: &Path, state_dir: &Path) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .current_dir(cwd)
        .env("HERDR_PLUGIN_STATE_DIR", state_dir)
        .env("HERDR_PLUGIN_CONFIG_DIR", state_dir.join("config"))
        // No socket: these verbs must work, or fail cleanly, without a server.
        .env("HERDR_SOCKET_PATH", state_dir.join("absent.sock"))
        .output()
        .expect("run the plugin binary")
}

#[test]
fn running_the_plugin_leaves_a_repository_byte_for_byte_unchanged() {
    let temp = TempDir::new("repo");
    let Some(repo) = fixture_repo(temp.path()) else {
        eprintln!("git unavailable; skipping");
        return;
    };
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    let before = fingerprint(&repo);
    assert!(
        !before.is_empty(),
        "the fingerprint is empty, so this test would pass vacuously"
    );
    // It must include the git internals, or it is not proving much.
    assert!(
        before.keys().any(|p| p.starts_with(".git")),
        "fingerprint does not cover .git"
    );

    // Every verb that does not require a live server, run with the repository as
    // the working directory — which is what herdr does for a plugin pane.
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["--once"],
        vec!["--json"],
        vec!["--forget"],
        vec!["--restore"],
    ] {
        run_plugin(&args, &repo, &state);
    }

    let after = fingerprint(&repo);
    assert_eq!(
        before, after,
        "the plugin modified the repository it was run inside"
    );
}

#[test]
fn the_plugin_writes_nothing_outside_its_state_directory() {
    let temp = TempDir::new("confine");
    let Some(repo) = fixture_repo(temp.path()) else {
        eprintln!("git unavailable; skipping");
        return;
    };
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    // A sibling directory that nothing should ever touch.
    let bystander = temp.path().join("bystander");
    std::fs::create_dir_all(&bystander).unwrap();
    std::fs::write(bystander.join("keep.txt"), "keep\n").unwrap();

    let before = fingerprint(&bystander);
    for args in [vec!["--once"], vec!["--json"], vec!["--forget"]] {
        run_plugin(&args, &repo, &state);
    }
    assert_eq!(
        before,
        fingerprint(&bystander),
        "the plugin wrote outside its state directory"
    );
}

#[test]
fn the_plugin_shells_out_only_to_itself_and_to_herdr() {
    // A source-level guard rather than a runtime one: a subprocess that only
    // spawns on a path this suite does not exercise would slip past any test
    // that merely watches a run. The rule is small enough to state exactly —
    // the daemon re-execs this binary, setup asks herdr to reload its config,
    // and supervision hands the sampler to the platform's own supervisor.
    // Nothing else, and in particular never `git`.
    // Asserting the exact *set* of spawn sites, rather than searching each line
    // for a permitted substring: a loose `line.contains("bin")` would wave
    // through `Command::new(user_supplied_binary)` without a murmur.
    const SANCTIONED: [&str; 3] = [
        // setup.rs — `herdr server reload-config`, so sidebar rows take effect
        // without the user restarting herdr.
        "src/setup.rs: Command::new(bin)",
        // daemon.rs — this binary re-execing itself as a detached `--daemon`.
        "src/daemon.rs: Command::new(exe)",
        // supervise.rs — `systemctl --user` or `launchctl`, and only from the
        // two verbs the user types to install and remove supervision. The
        // program is one of those two names built here, never a value from a
        // config file or a snapshot.
        "src/supervise.rs: Command::new(program)",
    ];

    let src = repo_root().join("src");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&src).expect("read src").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read source");

        for line in text.lines() {
            if let Some(at) = line.find("Command::new") {
                // Normalise `std::process::Command::new(x)` and `Command::new(x)`
                // to one spelling, and keep the argument expression verbatim.
                let call = line[at..].split(';').next().unwrap_or("").trim();
                found.push(format!("src/{name}: {call}"));
            }
        }

        assert!(
            !text.contains("\"git\""),
            "src/{name} names the git binary; this plugin must never invoke git"
        );
    }

    found.sort();
    let mut expected: Vec<String> = SANCTIONED.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "the set of subprocess spawn sites changed; each one needs a deliberate \
         review before it is added to SANCTIONED"
    );
}

#[test]
fn the_dependency_tree_cannot_reach_the_network() {
    // The plugin must make no network calls: no telemetry, no GitHub API, no
    // update checks. The strongest cheap proof is that nothing in the locked
    // dependency tree can open a socket in the first place.
    //
    // Asserting the whole allowlist rather than searching for known-bad names
    // means a *new* dependency has to be considered deliberately, instead of
    // slipping in because nobody thought to add it to a denylist.
    //
    // The list is checked against `Cargo.lock`, which records every platform's
    // dependencies rather than only this host's. That is deliberate: the lockfile
    // is what a supply-chain review reads, and a crate that is harmless because
    // it never compiles here should still be a decision somebody made on purpose.
    //
    // `windows-link` and `windows-sys` are exactly that case. They arrive via
    // `signal-hook -> signal-hook-registry -> errno`, are gated on Windows
    // targets, and appear in no dependency graph this plugin ever builds —
    // `cargo tree --target x86_64-unknown-linux-gnu` does not mention them, and
    // the manifest declares `platforms = ["linux", "macos"]`. They are Win32 API
    // bindings, not a network stack.
    const ALLOWED: [&str; 19] = [
        "windows-link",
        "windows-sys",
        "pulse",
        "serde",
        "serde_core",
        "serde_derive",
        "serde_json",
        "libc",
        "signal-hook",
        "signal-hook-registry",
        "errno",
        "itoa",
        "memchr",
        "proc-macro2",
        "quote",
        "syn",
        "unicode-ident",
        "zmij",
        "ryu",
    ];
    let lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).expect("read Cargo.lock");
    let mut unexpected = Vec::new();
    for line in lock.lines() {
        let Some(name) = line.strip_prefix("name = ") else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        if !ALLOWED.contains(&name) {
            unexpected.push(name.to_string());
        }
    }
    assert!(
        unexpected.is_empty(),
        "dependencies not on the audited allowlist: {unexpected:?} — each must be \
         reviewed for network access before being added here"
    );
}

#[test]
fn no_verb_panics_without_a_server() {
    // A plugin action that panics prints a Rust backtrace into somebody's
    // sidebar. Every verb reachable without a socket must exit cleanly, whether
    // it succeeds or reports a problem.
    let temp = TempDir::new("nosrv");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    for args in VERBS {
        let out = run_plugin(&args, temp.path(), &state);
        assert_clean_exit(&out, &args, "an empty state directory");
    }
}

#[test]
fn a_since_window_is_read_as_a_window_or_refused_outright() {
    // The window is the one argument whose spelling can quietly mean something
    // else: `--since 2` meaning two seconds when two hours were wanted is a
    // plausible wrong answer, and a rejected unit is how it stays impossible.
    let temp = TempDir::new("since");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    for window in ["90s", "30m", "2h", "3d", "600"] {
        let out = run_plugin(&["--once", "--since", window], temp.path(), &state);
        assert_clean_exit(&out, &["--once", "--since", window], "a window");
        assert_eq!(
            out.status.code(),
            Some(0),
            "`pulse --once --since {window}` should be accepted: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Refused rather than defaulted: a window pulse does not understand is a
    // question it must not answer with the whole retention and no comment.
    for window in ["0", "2w", "later", "-5m", ""] {
        let out = run_plugin(&["--once", "--since", window], temp.path(), &state);
        assert_clean_exit(&out, &["--once", "--since", window], "a bad window");
        assert_eq!(
            out.status.code(),
            Some(1),
            "`pulse --once --since {window}` should be refused",
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--since"),
            "the refusal has to name the option: {stderr}"
        );
    }
}

#[test]
fn a_window_the_ring_cannot_honour_is_said_out_loud() {
    // The pane draws what it has. What it does not have has to be a sentence,
    // not a shorter row the reader is left to notice.
    let temp = TempDir::new("since-edges");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    let past = run_plugin(&["--once", "--since", "30d"], temp.path(), &state);
    let stderr = String::from_utf8_lossy(&past.stderr);
    assert!(
        stderr.contains("not kept"),
        "a window past retention says the time before it was not kept: {stderr}"
    );

    let narrow = run_plugin(&["--once", "--since", "5s"], temp.path(), &state);
    let stderr = String::from_utf8_lossy(&narrow.stderr);
    assert!(
        stderr.contains("shorter than one"),
        "a window under one bucket says it was widened: {stderr}"
    );

    // And the ordinary case says nothing at all about the window.
    let plain = run_plugin(&["--once", "--since", "30m"], temp.path(), &state);
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        !stderr.contains("not kept") && !stderr.contains("shorter than one"),
        "an honoured window is not worth a sentence: {stderr}"
    );
}

/// Every verb that needs no socket.
const VERBS: [[&str; 1]; 8] = [
    ["--help"],
    ["--version"],
    ["--once"],
    ["--json"],
    ["--week"],
    ["--forget"],
    ["--restore"],
    ["--bogus-verb"],
];

fn assert_clean_exit(out: &std::process::Output, args: &[&str], context: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "`pulse {}` panicked with {context}: {stderr}",
        args.join(" ")
    );
    // A `panic = "abort"` release build produces no message at all, so the
    // absence of "panicked at" is not by itself proof of a clean exit.
    let code = out.status.code();
    assert!(
        code.is_some(),
        "`pulse {}` was killed by a signal with {context} — a release build \
         aborts rather than unwinding, so this is what a panic looks like there",
        args.join(" ")
    );
}

/// Every hostile-file path lives behind a `history.json` or a `config.json` that
/// the plugin did not write, and the panic test above never put one there.
///
/// An adversarial review found exactly this gap: an unchecked add in the store's
/// ring arithmetic panicked on a `newest_bucket` near `u64::MAX`, reachable from
/// `pulse --once` — a verb that runs inside somebody's sidebar — and no test ever
/// ran a verb with a state file present.
#[test]
fn no_verb_panics_on_a_hostile_state_file() {
    // Each is a plausible corruption or a deliberate extreme, not random noise:
    // a truncated write, a value at the type's boundary, a wrong-typed field, a
    // future format, and a structurally valid file with absurd numbers.
    let histories: [(&str, &str); 8] = [
        ("empty", ""),
        ("not-json", "this is not json at all"),
        (
            "truncated",
            "{\"version\":1,\"bucket_seconds\":60,\"workspaces\":[{",
        ),
        ("null", "null"),
        ("wrong-root-type", "[1, 2, 3]"),
        (
            "newest-bucket-at-u64-max",
            "{\"version\":1,\"bucket_seconds\":60,\"workspaces\":[{\"workspace_id\":\"w1\",\
             \"label\":\"a\",\"buckets\":[{\"samples\":1,\"working\":1,\"blocked\":0,\
             \"transitions\":0}],\"newest_bucket\":18446744073709551615,\"state\":\"working\",\
             \"state_since\":0,\"last_seen\":0,\"agent_seqs\":[]}]}",
        ),
        (
            "counts-exceed-samples",
            "{\"version\":1,\"bucket_seconds\":60,\"workspaces\":[{\"workspace_id\":\"w1\",\
             \"label\":\"a\",\"buckets\":[{\"samples\":1,\"working\":65535,\"blocked\":65535,\
             \"transitions\":65535}],\"newest_bucket\":1,\"state\":\"nonsense\",\
             \"state_since\":18446744073709551615,\"last_seen\":18446744073709551615,\
             \"agent_seqs\":[]}]}",
        ),
        (
            "future-format-version",
            "{\"version\":4294967295,\"bucket_seconds\":60,\"workspaces\":[]}",
        ),
    ];

    // Config is the other file the plugin reads but does not write.
    let configs: [(&str, &str); 4] = [
        (
            "all-u64-max",
            "{\"interval_seconds\":18446744073709551615,\
          \"bucket_seconds\":18446744073709551615,\
          \"badge_window_minutes\":18446744073709551615,\
          \"max_workspaces\":18446744073709551615,\
          \"retention_buckets\":18446744073709551615,\
          \"badge_columns\":18446744073709551615}",
        ),
        (
            "all-zero",
            "{\"interval_seconds\":0,\"bucket_seconds\":0,\"retention_buckets\":0,\
          \"badge_columns\":0,\"badge_window_minutes\":0,\"max_workspaces\":0}",
        ),
        (
            "wrong-types",
            "{\"interval_seconds\":\"soon\",\"badge_columns\":[]}",
        ),
        ("not-json", "{{{"),
    ];

    for (history_name, history) in histories {
        for (config_name, config) in &configs {
            let temp = TempDir::new(&format!("hostile-{history_name}-{config_name}"));
            let state = temp.path().join("state");
            std::fs::create_dir_all(state.join("config")).unwrap();
            std::fs::write(state.join("history.json"), history).unwrap();
            std::fs::write(state.join("config").join("config.json"), config).unwrap();

            for args in VERBS {
                let out = run_plugin(&args, temp.path(), &state);
                assert_clean_exit(
                    &out,
                    &args,
                    &format!("history `{history_name}` and config `{config_name}`"),
                );
            }
        }
    }
}
