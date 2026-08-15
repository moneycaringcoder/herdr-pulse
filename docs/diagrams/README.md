# Diagram sources

Sources live here, rendered output in `docs/img/`. Both are committed, so a
reader never needs a toolchain to see the picture and a maintainer never has to
reverse-engineer an SVG to change one.

Regenerate after editing a source:

```sh
d2 --theme 0 --dark-theme 200 --pad 24 docs/diagrams/mechanism.d2 docs/img/mechanism.svg
```

The `--dark-theme` flag matters: it emits one SVG carrying both palettes behind a
`prefers-color-scheme` query, so a single file reads correctly on GitHub's light
and dark themes.

`docs/img/logo.svg` is hand-authored rather than generated. Its colours are
mid-tone on purpose so it needs no dark variant.

Avoid Unicode marks in d2 labels — the bundled font has no glyph for the block
elements this plugin renders, and they come out as tofu. Describe the sparkline
in words inside a diagram; show the real thing in the README instead.
