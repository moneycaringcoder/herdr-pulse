## What this changes

<!-- One or two sentences. Why, more than what. -->

## How you checked it

<!--
A green suite is necessary and not sufficient here — this project has shipped
invisible wrong answers past a fully green suite before. If the change affects
what a user sees, say what you observed when you ran it for real.
-->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] Ran it against a live herdr session and looked at the result

## Safety

- [ ] Writes nothing outside the plugin's state directory
- [ ] Adds no subprocess, and no git invocation
- [ ] Adds no dependency that can reach the network (`tests/read_only.rs`
      asserts the full allowlist — a new transitive crate needs a deliberate
      review, not just an allowlist entry)
- [ ] Does not let an unobserved bucket render as a quiet one

<!--
If any box is unchecked, say why here rather than deleting it. An honest
"I did not test this on macOS" is far more useful than a checked box.
-->
