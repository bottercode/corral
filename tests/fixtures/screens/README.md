# Screen fixtures

Byte streams fed through vt100 by `tests/rules.rs` to check the screen rules
in `src/detect/rules.toml`.

- `claude-trust.bytes` was recorded from a real Claude Code session with
  `corral debug dump p1 -o claude-trust.bytes`.
- The others are hand-written reproductions of each agent's screen (same
  text and layout, escape sequences included).

Record new fixtures the same way: start the agent in corral, bring it to the
state you want, and run `corral debug dump <pane> -o name.bytes`.
