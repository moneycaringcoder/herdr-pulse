# Contributing to pulse

Contributions are genuinely welcome — bug reports, questions, documentation
fixes, and code. This document exists so you know what to expect before you spend
time on something, not to put obstacles in front of you.

The project is maintained by one person. That means review is attentive but not
instant, and it means every change is read carefully before it lands. Please
don't take questions on a pull request as resistance; they are how the maintainer
stays confident in code that runs unattended on other people's machines.

## The rules that matter

**pulse observes and never acts.** It reads the herdr socket and writes badge
tokens. It does not prompt agents, send keys, kill processes, or touch a
repository. A pull request that adds any of those is a different plugin, and the
answer will be no however good the code is.

Five properties are enforced by tests, and if your change makes one of them fail
the test is right and the change is wrong:

| Property | Enforced by |
|---|---|
| Nothing is written outside the plugin's state directory, except the unit `--supervise` installs where the user asked for it | `tests/read_only.rs` |
| No git invocation, and no subprocess beyond the daemon re-exec, `herdr server reload-config`, and the supervisor `--supervise` hands the sampler to | `tests/read_only.rs` |
| No dependency that can open a network socket | `tests/read_only.rs` (audited allowlist over `Cargo.lock`) |
| Every `--json` key path, with its type and nullability, is pinned | `tests/render.rs` (54 paths over two fixtures) |
| `null` and `0` stay distinct in every `--json` array | `tests/render.rs` |

**A gap is not a quiet period.** This is the correctness property that the whole
design turns on. A bucket the sampler did not observe renders as a gap glyph,
never as a zero bar. If pulse was not watching for forty minutes, it must say so
rather than implying the workspace was quiet. Any change that lets "we were not
watching" render as "nothing happened" is a bug of the worst kind here: an
invisible wrong answer that looks exactly like a right one.

**Prefer a loud error to a quiet fallback.** "No workspaces found" and "I could
not parse the response" must never look the same. A silent degradation in this
plugin renders as a plausible sparkline that is simply untrue.

## Getting set up

```sh
git clone https://github.com/moneycaringcoder/herdr-pulse
cd herdr-pulse
cargo build --release
herdr plugin link .          # note: `link` does NOT run the build step
```

Then run it by hand rather than only through herdr — the verbs are designed to
work from a shell:

```sh
./target/release/pulse --enable
./target/release/pulse --once
./target/release/pulse --disable
```

## Before you open a pull request

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

CI runs these four gates on Linux and macOS. The local toolchain is expected to
match CI, so a lint or release-build failure you cannot reproduce locally is a
real difference worth investigating rather than something to paper over with an
`#[allow]`.

## Pull request flow

Keep one issue and one focused implementation PR together. Topic branches
usually use `pulse/<short-topic>`, but that is a convention rather than a gate.
Open the implementation PR non-draft, link it with `Closes #N`, fill in the
safety checklist, and include the commands above plus live Herdr evidence for
anything user-visible. Merge only after the actual repository checks and review
criteria shown by GitHub are satisfied; this document does not claim unverified
branch-protection or approval rules.

## Releases

Prepare a separate non-draft `release/vX.Y.Z` PR after the intended changes are
on `main`. Move these four surfaces together:

- `Cargo.toml` package version;
- Pulse's entry in `Cargo.lock`;
- `herdr-plugin.toml` version; and
- a dated, non-empty `CHANGELOG.md` section.

Rehearse the identity gate before merge:

```sh
python3 tools/check_release.py --tag vX.Y.Z
```

After that PR is reviewed, green and merged, push the exact stable tag:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag starts the Release workflow. Wait for its identity job and both Linux
and macOS gate matrices, then confirm the GitHub release exists and its notes
match that version's changelog section. The daily Herdr-master canary remains
advisory and is not a release or PR gate.

## On tests

A green suite is necessary and not sufficient, and this project has the scars to
prove it. Two habits are expected:

- **Build fixtures from captured output, not from assumptions.** `tests/data/`
  holds a real `session.snapshot`, and its README says exactly which values were
  sanitized and which fixture is derived rather than captured. A fake that
  replies in the shape your code expects, rather than the shape herdr actually
  sends, tests nothing at all.
- **Test the degenerate cases.** Empty series, one bucket, all-zero, all-max, a
  series longer than the display width, a series that is entirely gaps, a clock
  that goes backwards, a ring that has wrapped twice. That is where this kind of
  code goes wrong, not in the happy path.

If you change rendering, check the result in a real narrow sidebar. Alignment and
column counts are the product here, not polish.

If you change the `--json` document, read
[docs/json-schema.md](docs/json-schema.md) first. The shape is a promise to
anything scripting against it, and the two rules are short: a change a consumer
could notice by reading the keys bumps `JSON_SCHEMA_VERSION`, and every visible
change earns a changelog entry. The pinned list in `tests/render.rs` will fail
the moment a key or a type moves — but it cannot tell whether the version should
move with it, and it cannot see a unit or a meaning change at all. That decision
is the reviewer's, which is why it is written down rather than automated.

## Commit messages

Plain prose, present tense, explaining why rather than what. No trailers
attributing the work to a tool.

## Code of conduct

Participation is covered by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
