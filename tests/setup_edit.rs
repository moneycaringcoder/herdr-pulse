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
