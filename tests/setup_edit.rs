//! Tests for the `config.toml` splice.
//!
//! This module edits a file it does not own, on behalf of a user who cannot see
//! the result until they look at their sidebar — so every failure mode here is
//! quiet by nature. An adversarial review found it shipped with no test coverage
//! at all, and two ordinary config shapes silently took a path that printed
//! "already configured; nothing to do" and exited 0 while changing nothing.
//!
//! The property that matters most is structural: the entries must land **inside**
//! a row of the `rows` array, never beside one. A table dropped between two rows
//! is still valid TOML, so herdr accepts the file and then renders nothing —
//! there is no error anywhere for the user to find.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use pulse::setup::{plan_edit, Plan};

/// The token names the splice is supposed to introduce.
const TOKENS: [&str; 3] = ["$pulse_blocked", "$pulse_working", "$pulse_quiet"];

fn edited(text: &str) -> String {
    match plan_edit(text) {
        Plan::Edit(out) => out,
        other => panic!("expected an edit, got {other:?}\n--- input ---\n{text}"),
    }
}

/// Every token appears exactly once.
fn assert_tokens_present_once(out: &str) {
    for token in TOKENS {
        let quoted = format!("\"{token}\"");
        let count = out.matches(&quoted).count();
        assert_eq!(
            count, 1,
            "expected {quoted} exactly once, found {count}\n{out}"
        );
    }
}

/// Walks the `rows` array and asserts every one of our tokens sits at bracket
/// depth 2 — inside a row — rather than at depth 1, beside one.
///
/// This is the check that a "looks fine" file would pass and a silently useless
/// one would fail, and it is deliberately independent of the code under test: it
/// re-derives depth from the output text rather than trusting anything the
/// splice recorded.
fn assert_tokens_are_inside_a_row(out: &str) {
    let start = out.find("rows").expect("no rows key in output");
    let open = out[start..].find('[').expect("no rows array") + start;

    let mut depth = 0usize;
    let mut checked = 0usize;
    let mut current: Option<usize> = None;

    for (i, ch) in out[open..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break; // rows array closed
                }
            }
            _ => {}
        }
        // Note the depth at the start of each token occurrence.
        for token in TOKENS {
            let needle = format!("\"{token}\"");
            if out[open + i..].starts_with(&needle) {
                current = Some(depth);
                assert_eq!(
                    depth, 2,
                    "{token} sits at bracket depth {depth}, not inside a row (depth 2).\n\
                     herdr would accept this file and render nothing.\n{out}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(
        checked,
        TOKENS.len(),
        "not every token was found in the rows array\n{out}"
    );
    assert!(current.is_some());
}

#[test]
fn a_file_that_already_names_our_tokens_is_left_alone() {
    let text = "\
[ui.sidebar.spaces]
rows = [
  [\"state_icon\", \"workspace\"],
  [\"branch\",
    { token = \"$pulse_working\", fg = \"#8CD98C\" },
  ],
]
";
    assert_eq!(plan_edit(text), Plan::AlreadyConfigured);
}

#[test]
fn a_config_with_no_sidebar_section_gets_a_complete_one() {
    let text = "[general]\ntheme = \"dark\"\n";
    let out = edited(text);
    assert!(out.contains("[ui.sidebar.spaces]"), "{out}");
    assert!(
        out.starts_with("[general]"),
        "existing config was disturbed\n{out}"
    );
    assert_tokens_present_once(&out);
    assert_tokens_are_inside_a_row(&out);
}

#[test]
fn an_entirely_empty_config_still_gets_a_working_section() {
    let out = edited("");
    assert_tokens_present_once(&out);
    assert_tokens_are_inside_a_row(&out);
}

#[test]
fn entries_land_inside_the_last_row_of_a_multi_line_rows_array() {
    let text = "\
[ui.sidebar.spaces]
rows = [
  [\"state_icon\", \"workspace\"],
  [\"branch\",
    { token = \"$git_dirty\", fg = \"#f9e2af\" },
  ],
]
";
    let out = edited(text);
    assert_tokens_present_once(&out);
    assert_tokens_are_inside_a_row(&out);
    // Another plugin's token must survive untouched.
    assert!(
        out.contains("$git_dirty"),
        "an existing token was lost\n{out}"
    );
}

/// The shape that used to be silently unconfigurable.
///
/// `rows = [["a"],["b"]]` has three opening and three closing brackets, so the
/// old bracket-counting shortcut computed a depth of zero for the line, skipped
/// it, and found no row at all — after which `--setup` reported success and did
/// nothing.
#[test]
fn a_rows_array_written_on_one_line_is_still_editable() {
    let text = "[ui.sidebar.spaces]\nrows = [[\"state_icon\", \"workspace\"], [\"branch\"]]\n";
    let out = edited(text);
    assert_tokens_present_once(&out);
    assert_tokens_are_inside_a_row(&out);
    assert!(
        out.contains("state_icon"),
        "an existing row was lost\n{out}"
    );
}

/// A single row, on one line, is the tightest version of the same case: the
/// bracket that closes the row and the bracket that closes `rows` are adjacent,
/// and splicing at the wrong one puts our entries outside every row.
#[test]
fn a_single_row_on_one_line_splices_inside_the_row_not_beside_it() {
    let text = "[ui.sidebar.spaces]\nrows = [[\"branch\"]]\n";
    let out = edited(text);
    assert_tokens_present_once(&out);
    assert_tokens_are_inside_a_row(&out);
}

#[test]
fn a_sidebar_section_with_no_rows_key_is_refused_rather_than_reported_as_done() {
    let text = "[ui.sidebar.spaces]\nwidth = 30\n";
    assert_eq!(
        plan_edit(text),
        Plan::NoRowToEdit,
        "a section we cannot edit must not be reported as already configured"
    );
}

#[test]
fn a_refusal_is_distinguishable_from_a_no_op() {
    // The whole point of the three-way result: these two must never collapse
    // into one another, because one means "you are set up" and the other means
    // "you are not set up and I could not fix it".
    let configured = "[ui.sidebar.spaces]\nrows = [[\"branch\", { token = \"$pulse_quiet\" }]]\n";
    let unfixable = "[ui.sidebar.spaces]\nwidth = 30\n";
    assert_eq!(plan_edit(configured), Plan::AlreadyConfigured);
    assert_eq!(plan_edit(unfixable), Plan::NoRowToEdit);
    assert_ne!(plan_edit(configured), plan_edit(unfixable));
}

#[test]
fn running_the_splice_twice_is_a_no_op() {
    for text in [
        "[general]\ntheme = \"dark\"\n",
        "[ui.sidebar.spaces]\nrows = [\n  [\"branch\",\n  ],\n]\n",
        "[ui.sidebar.spaces]\nrows = [[\"state_icon\"], [\"branch\"]]\n",
        "",
    ] {
        let once = edited(text);
        assert_eq!(
            plan_edit(&once),
            Plan::AlreadyConfigured,
            "a second run would edit again, duplicating the entries\n{once}"
        );
    }
}

#[test]
fn a_later_table_is_never_mistaken_for_part_of_the_sidebar_section() {
    let text = "\
[ui.sidebar.spaces]
width = 30

[ui.other]
rows = [
  [\"something\"],
]
";
    // The `rows` here belongs to `[ui.other]`, not to us. Editing it would
    // silently rewrite an unrelated part of the user's config.
    assert_eq!(plan_edit(text), Plan::NoRowToEdit);
}

#[test]
fn the_files_trailing_newline_convention_is_preserved() {
    let with = "[ui.sidebar.spaces]\nrows = [\n  [\"branch\",\n  ],\n]\n";
    let without = "[ui.sidebar.spaces]\nrows = [\n  [\"branch\",\n  ],\n]";
    assert!(edited(with).ends_with('\n'), "trailing newline was dropped");
    assert!(
        !edited(without).ends_with('\n'),
        "a trailing newline was invented"
    );
}

#[test]
fn nothing_is_ever_deleted() {
    // The edit is additive by contract. Every non-empty line of the original
    // must still be present, so a bug can add noise but can never lose a user's
    // configuration.
    let text = "\
[general]
theme = \"dark\"

[ui.sidebar.spaces]
width = 30
rows = [
  [\"state_icon\", \"workspace\"],
  [\"branch\",
    { token = \"$git_dirty\", fg = \"#f9e2af\" },
  ],
]

[keys]
leader = \"ctrl+a\"
";
    let out = edited(text);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            out.contains(line.trim()),
            "line was lost from the user's config: {line:?}\n{out}"
        );
    }
}

const ORIGINAL: &str = "[general]\ntheme = \"dark\"\n";

struct SetupHarness {
    root: PathBuf,
    config: PathBuf,
    herdr: PathBuf,
    log: PathBuf,
}

impl SetupHarness {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "pulse-setup-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        let config = root.join("config.toml");
        std::fs::write(&config, ORIGINAL).expect("config");
        let herdr = root.join("herdr");
        std::fs::write(
            &herdr,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PULSE_TEST_LOG\"\nprintf '%s' \
             \"$PULSE_TEST_DIAGNOSTIC\" >&2\nexit \"$PULSE_TEST_EXIT\"\n",
        )
        .expect("fake herdr");
        std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o755))
            .expect("fake herdr mode");
        Self {
            log: root.join("reload.log"),
            root,
            config,
            herdr,
        }
    }

    fn backup(&self) -> PathBuf {
        suffixed(&self.config, ".pulse-backup")
    }

    fn metadata(&self) -> PathBuf {
        suffixed(&self.backup(), ".meta")
    }

    fn run(&self, args: &[&str], exit: i32, diagnostic: &str) -> Output {
        self.run_with_bin(args, &self.herdr, exit, diagnostic)
    }

    fn run_with_bin(&self, args: &[&str], bin: &Path, exit: i32, diagnostic: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pulse"))
            .args(args)
            .env("HERDR_CONFIG_PATH", &self.config)
            .env("HERDR_BIN_PATH", bin)
            .env("PULSE_TEST_LOG", &self.log)
            .env("PULSE_TEST_EXIT", exit.to_string())
            .env("PULSE_TEST_DIAGNOSTIC", diagnostic)
            .output()
            .expect("run pulse setup command")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for SetupHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn setup_and_safe_rollback_are_complete_transactions() {
    let harness = SetupHarness::new("success");

    let setup = harness.run(&["--setup"], 0, "");
    assert!(setup.status.success(), "{}", stderr(&setup));
    assert_eq!(
        std::fs::read_to_string(&harness.config).unwrap(),
        edited(ORIGINAL)
    );
    assert_eq!(std::fs::read_to_string(harness.backup()).unwrap(), ORIGINAL);
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(harness.metadata()).unwrap()).unwrap();
    assert_eq!(metadata["version"], 1);
    assert_eq!(metadata["original"], ORIGINAL);
    assert_eq!(metadata["installed"], edited(ORIGINAL));
    assert_eq!(metadata["reloaded"], true);
    let repeated = harness.run(&["--setup"], 9, "must not be called");
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert_eq!(
        harness.log(),
        "server reload-config\n",
        "a completed setup remains a no-op"
    );

    let rollback = harness.run(&["--setup-rollback"], 0, "");
    assert!(rollback.status.success(), "{}", stderr(&rollback));
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(!harness.backup().exists());
    assert!(!harness.metadata().exists());
    assert_eq!(
        harness.log(),
        "server reload-config\nserver reload-config\n"
    );
}

#[test]
fn rejected_setup_restores_the_original_and_surfaces_herdrs_diagnostic() {
    let harness = SetupHarness::new("rejected");

    let output = harness.run(&["--setup"], 7, "invalid sidebar");

    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(!harness.backup().exists());
    assert!(!harness.metadata().exists());
    let error = stderr(&output);
    assert!(error.contains("herdr rejected"));
    assert!(error.contains("invalid sidebar"));
}

#[test]
fn launch_failure_keeps_the_valid_edit_and_recovery_files() {
    let harness = SetupHarness::new("launch");
    let missing = harness.root.join("missing-herdr");

    let output = harness.run_with_bin(&["--setup"], &missing, 0, "");

    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(&harness.config).unwrap(),
        edited(ORIGINAL)
    );
    assert_eq!(std::fs::read_to_string(harness.backup()).unwrap(), ORIGINAL);
    assert!(harness.metadata().exists());
    let error = stderr(&output);
    assert!(error.contains("could not launch"));
    assert!(error.contains("server reload-config"));
    assert!(!error.contains("herdr rejected"));

    let rejected = harness.run(&["--setup"], 8, "server still absent");
    assert!(!rejected.status.success());
    assert_eq!(
        std::fs::read_to_string(&harness.config).unwrap(),
        edited(ORIGINAL)
    );
    assert!(harness.backup().exists());
    assert!(harness.metadata().exists());
    assert!(stderr(&rejected).contains("pending sidebar reload"));

    let retry = harness.run(&["--setup"], 0, "");
    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(String::from_utf8_lossy(&retry.stdout).contains("completed the pending sidebar reload"));
    assert_eq!(
        harness.log(),
        "server reload-config\nserver reload-config\n"
    );
    assert!(harness.backup().exists());
    assert!(harness.metadata().exists());
}

#[test]
fn rollback_refuses_to_overwrite_changes_made_after_setup() {
    let harness = SetupHarness::new("later-edit");
    assert!(harness.run(&["--setup"], 0, "").status.success());
    let edited_later = format!("{}\n[keys]\nleader = \"ctrl+a\"\n", edited(ORIGINAL));
    std::fs::write(&harness.config, &edited_later).unwrap();

    let rollback = harness.run(&["--setup-rollback"], 0, "");

    assert!(!rollback.status.success());
    assert_eq!(
        std::fs::read_to_string(&harness.config).unwrap(),
        edited_later
    );
    assert!(harness.backup().exists());
    assert!(harness.metadata().exists());
    assert_eq!(harness.log(), "server reload-config\n");
    let error = stderr(&rollback);
    assert!(error.contains("changed after pulse setup"));
    assert!(error.contains("$pulse_blocked"));
    assert!(error.contains("Do not copy the backup"));
}

#[test]
fn rejected_rollback_restores_the_installed_config_and_recovery_files() {
    let harness = SetupHarness::new("rollback-rejected");
    assert!(harness.run(&["--setup"], 0, "").status.success());
    let installed = std::fs::read_to_string(&harness.config).unwrap();

    let rollback = harness.run(&["--setup-rollback"], 9, "reload refused");

    assert!(!rollback.status.success());
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), installed);
    assert!(harness.backup().exists());
    assert!(harness.metadata().exists());
    assert!(stderr(&rollback).contains("reload refused"));
}

#[test]
fn rollback_launch_failure_leaves_the_requested_file_and_can_clean_up_later() {
    let harness = SetupHarness::new("rollback-launch");
    assert!(harness.run(&["--setup"], 0, "").status.success());
    let missing = harness.root.join("missing-herdr");

    let rollback = harness.run_with_bin(&["--setup-rollback"], &missing, 0, "");
    assert!(!rollback.status.success());
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(harness.backup().exists());
    assert!(harness.metadata().exists());
    assert!(stderr(&rollback).contains("rollback is complete on disk"));

    let cleanup = harness.run(&["--setup-rollback"], 0, "");
    assert!(cleanup.status.success(), "{}", stderr(&cleanup));
    assert!(!harness.backup().exists());
    assert!(!harness.metadata().exists());
    assert_eq!(harness.log(), "server reload-config\n");
}

#[test]
fn rollback_recovers_an_interruption_after_transaction_publication() {
    let harness = SetupHarness::new("interrupted");
    assert!(harness.run(&["--setup"], 0, "").status.success());
    std::fs::write(&harness.config, ORIGINAL).expect("simulate restored config");

    let rollback = harness.run(&["--setup-rollback"], 0, "");

    assert!(rollback.status.success(), "{}", stderr(&rollback));
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(!harness.backup().exists());
    assert!(!harness.metadata().exists());
    assert_eq!(
        harness.log(),
        "server reload-config\n",
        "an already-restored transaction needs no second reload"
    );
}

#[test]
fn existing_or_legacy_recovery_files_are_never_clobbered() {
    let harness = SetupHarness::new("existing");
    std::fs::write(harness.backup(), b"keep me").unwrap();

    let setup = harness.run(&["--setup"], 0, "");
    assert!(!setup.status.success());
    assert_eq!(std::fs::read(harness.backup()).unwrap(), b"keep me");
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(harness.log().is_empty());
}

#[test]
fn legacy_backup_without_metadata_remains_rollback_compatible() {
    let harness = SetupHarness::new("legacy");
    std::fs::write(harness.backup(), ORIGINAL).unwrap();
    std::fs::write(&harness.config, edited(ORIGINAL)).unwrap();

    let rollback = harness.run(&["--setup-rollback"], 0, "");

    assert!(rollback.status.success(), "{}", stderr(&rollback));
    assert_eq!(std::fs::read_to_string(&harness.config).unwrap(), ORIGINAL);
    assert!(!harness.backup().exists());
}
