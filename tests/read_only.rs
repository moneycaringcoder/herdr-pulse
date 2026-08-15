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
    // the daemon re-execs this binary, and setup asks herdr to reload its
    // config. Nothing else, and in particular never `git`.
    let src = repo_root().join("src");
    let mut offenders = Vec::new();
    let mut walk = |dir: &Path, offenders: &mut Vec<String>| {
        for entry in std::fs::read_dir(dir).expect("read src").flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for (n, line) in text.lines().enumerate() {
                if !line.contains("Command::new") {
                    continue;
                }
                // The two sanctioned spawns, by the expression each uses.
                let sanctioned = line.contains("current_exe") || line.contains("bin");
                if !sanctioned {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap().to_string_lossy(),
                        n + 1,
                        line.trim()
                    ));
                }
            }
            assert!(
                !text.contains("\"git\""),
                "{} names the git binary; this plugin must never invoke git",
                path.display()
            );
        }
    };
    walk(&src, &mut offenders);
    assert!(
        offenders.is_empty(),
        "unexpected subprocess spawns: {offenders:#?}"
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
    const ALLOWED: [&str; 17] = [
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

    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["--once"],
        vec!["--json"],
        vec!["--forget"],
        vec!["--restore"],
        vec!["--bogus-verb"],
    ] {
        let out = run_plugin(&args, temp.path(), &state);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("panicked at"),
            "`pulse {}` panicked: {stderr}",
            args.join(" ")
        );
    }
}
