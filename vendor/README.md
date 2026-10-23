# Vendored crates

## vt100 0.16.2

From [crates.io](https://crates.io/crates/vt100) (MIT, see `vt100/LICENSE`),
used through `[patch.crates-io]` in `Cargo.toml` so corral and `tui-term`
share it. One change, in `src/grid.rs` (`Grid::scroll_up`):

Upstream keeps a line in scrollback only when no scroll region is set. Real
terminals (xterm, kitty, Ghostty, iTerm2) also keep it when the scroll region
starts at the top row. Codex writes its chat history that way, inserting lines
above its inline view with `ESC[1;Nr` and a line feed, so without this change
its history never reached corral's scrollback and couldn't be scrolled to.
