# corral

A terminal runtime for coding agents. Run Claude Code, Codex, opencode and
friends side by side, see at a glance which one is waiting for you, and keep
them running when you close the laptop or lose SSH. Optionally, give each
agent its own git worktree so parallel agents never edit the same checkout.

```
 corral    ≡ menu  ✕ exit  « │ 2 fixer · Fix token refresh   ◆ blocked 4m│▍1 api · Add rate limiting         ⠋ 32s
 ───────────────── ⇅ by need │⏺ I'll patch the refresh lock.             │⏺ Reading src/limits.rs
 NEEDS YOU  2                │                                            │
 2 fixer       ◆ blocked 4m  │ Do you want to proceed?                    │  ⎿ Read 120 lines
 3 docs           ✓ done 1m  │ ❯ 1. Yes                                   │
                             │   2. No                                    │⏺ Adding a token bucket
 WORKING  1                  │                                            │
▍1 api               ⠋ 32s   │                                            │
▍  Add rate limiting         │                                            │
▍  api · claude              │                                            │
▍  ⎇ corral/api-81c2 +42 −7  │                                            │
                             │                                            │
 QUIET  1                    │                                            │
 4 tests               idle  │                                            │
 ─────────────────────────── │                                            │
 + new shell   ⎇ worktree    │                                            │          ^b ? keys
```

## Why

Running several agents in separate terminal tabs breaks down quickly:

- **You miss agents that are blocked.** An agent asks for permission in a
  tab you're not looking at and sits idle for twenty minutes.
- **Closing the terminal kills the work.** Shutting the laptop lid or
  dropping an SSH connection ends every agent.
- **Parallel agents trip over each other.** Two agents editing the same
  checkout overwrite each other's changes and confuse each other's builds.

corral runs a small background server that owns every agent's terminal. The
UI is a client that attaches and detaches; the agents don't notice.

- **Who needs you, first.** The sidebar sorts agents by what they need:
  blocked or finished agents on top, then the ones working, then the quiet
  ones, each with how long it's been in that state. You also get a bell and a
  desktop notification.
- **Several agents on screen.** Show up to four agents side by side and type
  into whichever one you click.
- **A worktree per agent, if you want one.** `--worktree` starts an agent in a
  fresh git worktree on its own branch.
- **Review and land agents' work without leaving corral.** Press `^b D` to
  see the diff, then commit and merge it into your branch.

## Install

corral is not published yet. Build it from source (Rust 1.88+, macOS or
Linux):

```sh
git clone <this repo> corral && cd corral
cargo install --path .
```

This installs a `corral` binary into `~/.cargo/bin`.

## Quick start

```sh
corral                      # open the UI (starts the server if needed)
```

corral opens a shell in the current directory. Start an agent in it the way
you normally would (`claude`, `codex`, `opencode`…); corral notices and tracks
it. When the agent quits you're back at the shell, in the same place.

Everything you type goes to the focused agent. corral's own commands are
**Ctrl-b, then a key** (the bottom line reminds you):

| Keys | Action |
|---|---|
| `^b n` | new shell, next to the focused agent's work |
| `^b w` | new shell in its own git worktree (asks for a branch name) |
| `^b j` / `^b k`, `^b ↑` / `^b ↓` | next / previous agent |
| `^b 1` … `^b 9` | jump to an agent |
| `^b b` | jump to the next blocked agent |
| `^b s` | show another agent alongside (up to four on screen) |
| `^b S` | take the focused agent off screen (it keeps running) |
| `^b o` | only the focused agent on screen; again to bring the others back |
| `^b h` / `^b l`, `^b Tab` | move between the agents on screen |
| `^b c` | collapse the sidebar to agent numbers, or expand it |
| `^b D` | review the agent's worktree diff ([below](#diff-review)) |
| `^b x` | close the agent's pane |
| `^b r` | rename it |
| `^b m` | menu: theme, sidebar order, notifications, agent hooks |
| `^b d` | detach (agents keep running) |
| `^b q` | exit: detach, or stop corral and every agent (also the **✕ exit** button) |
| `^b [` | scrollback (`k`/`j`, PgUp/PgDn, `g`/`G`, `q`) |
| `^b ?` | all the keys |
| `^b ^b` | send a literal `Ctrl-b` to the agent |

Click an agent on screen to type into it, or a sidebar row to bring that agent
into the focused spot. The **+ new shell** / **⎇ worktree** buttons at the
bottom of the sidebar open one. At the top:

- **≡ menu** (or `^b m`) changes the theme, the sidebar's order and
  notifications, and installs agent hooks. Changes are saved to your config.
- **⇅ by need / by project**, on the line under the title, switches between
  listing agents by what they need from you and grouping them by project.
- **«** collapses the sidebar to a narrow rail of agent numbers and state
  marks (`◆` blocked, `✓` done, a spinner while working), giving the room to
  your agents; **»** brings it back. `^b c` does the same.
- **✕ exit** asks whether to detach, leaving agents running, or stop corral
  and every agent; worktrees are kept either way. Scroll the wheel for
scrollback. corral captures the mouse, so hold Shift (Option in some
terminals) to select text.

Detach, close the terminal, and come back later (or from another machine over
SSH): run `corral` again and everything is where you left it.

## Worktrees

In the UI, `^b w` (or **⎇ worktree**) asks for a branch name, leave it empty
for a fresh `corral/agent-xxxx`, and opens a shell in the new checkout. From
any shell:

```sh
corral new --worktree -- claude
corral new --worktree --branch fix/login -- codex
```

Each worktree is created next to, not inside, your repository:

```
~/.corral/worktrees/<repo>-<hash>/<slug>     on branch corral/<slug>
```

- Your own checkout stays untouched, and each agent works on its own branch.
- The sidebar shows the branch and the lines the agent has added and removed
  so far, committed and uncommitted (`⎇ corral/claude-3f9a +42 −7`). This
  updates every few seconds.
- The branch's starting point is recorded (in `branch.<name>.corralBase`), so
  diffs and merges know where to compare against.
- `--branch` uses that branch, creating it from `HEAD` if it doesn't exist
  yet or checking it out if it does.
- Closing a worktree pane asks whether to **keep** the worktree, **remove**
  it, or **remove it and delete the branch**.
  - Removing refuses while there are uncommitted changes.
  - Deleting the branch refuses while it has commits that aren't on any other
    branch.
  - Both can be forced, from the UI or with `--force`.
- Starting with `--worktree` outside a git repository prints a warning and
  starts the agent without one.

```sh
corral worktree ls                        # worktrees corral created, with their panes
corral worktree rm p3                     # by pane (once it has exited) …
corral worktree rm ~/.corral/worktrees/api-1a2b3c4d/claude-3f9a --delete-branch   # … or by path
corral kill p3 --rm-worktree              # close a pane and remove its worktree
corral kill p3 --delete-branch            # … and delete its branch
```

## Diff review

Focus a worktree agent and press `^b D` to see everything it changed since
its branch started:
- committed work
- uncommitted edits
- new untracked files

On the left is the list of files; on the right, the selected file's coloured
diff. From there:

| Key | Action |
|---|---|
| `j` / `k` | previous / next file |
| `space` / `K` | scroll the diff |
| `c` | commit everything in the worktree (with an editable message) |
| `m` | merge the branch into the branch it came from, in your checkout |
| `x` | discard uncommitted changes, after confirming (committed work is kept) |
| `r` | refresh |
| `q` | back |

Merging is careful. It refuses, and changes nothing, if any of these apply:
- the worktree has uncommitted work (commit it first)
- your checkout is on a different branch, or has uncommitted changes
- the merge would conflict (it is aborted)

It uses a merge commit (`Merge corral/<slug> (corral)`).

The same is available from scripts:

```sh
corral diff p2 [--stat] [--json]
corral commit p2 -m "Fix login redirect"
corral merge p2
corral discard p2 --yes
```

## Agent state

| State | Meaning |
|---|---|
| ◆ blocked | a permission prompt or question is on screen |
| ✓ done | it was working and stopped, and you haven't looked at it since |
| ⠋ working | output in the last moment, or a spinner / "esc to interrupt" on screen |
| idle | you've seen it, or it hasn't started anything |
| exited N | the process ended |

Blocked and done agents are listed under **NEEDS YOU**, working ones under
**WORKING**, the rest under **QUIET**. An agent counts as seen once it's on
screen.

corral recognises claude, codex, opencode, gemini, amp, cursor-agent, copilot,
pi, droid and grok from the pane's foreground process. It then reads their
prompts from the screen using
[data-driven rules](src/detect/rules.toml), plus a generic `(y/n)` / `[Y/n]`
rule for any program.

### Hooks (more accurate)

Screen heuristics can drift as agent UIs change. Agents that support hooks
can report their state directly:

```sh
corral integrate claude     # adds hooks to ~/.claude/settings.json
corral integrate codex      # sets `notify` in ~/.codex/config.toml
```

Both commands show the diff and ask before writing. They back up the
original file once, can be run again safely, and are undone with
`--uninstall`. The hooks only do anything inside corral panes (they check
`$CORRAL_PANE`), so agents started elsewhere are unaffected.

| Claude Code hook | Reports |
|---|---|
| Notification (permission prompts) | blocked |
| Stop | done |
| UserPromptSubmit, PreToolUse, PostToolUse | working |

A hook report wins over the heuristics until the next hook event. It also
gives way when it goes stale:
- a minute of further output, for any report
- typing an answer, for `blocked`
- 10 s without output, for `working`

Other tools can report the same way from inside a pane:
`corral report --state blocked|working|done|idle`.

### Notifications

When an agent you aren't looking at becomes blocked or done, every attached
client rings the bell and sends an OSC 9 desktop notification. iTerm2,
Ghostty, WezTerm and kitty show these; Terminal.app ignores them. Turn
notifications off with `notifications = false`.

## CLI

The CLI is also the API for scripts and for agents that drive other agents.
Read commands take `--json`. Panes can be given as `p3`, `3`, or a unique
name.

| Command | |
|---|---|
| `corral` / `corral attach [workspace]` | open the UI |
| `corral ls [--json]` | list panes |
| `corral new [--name N] [--cwd DIR] [--workspace W] [--worktree [--branch B]] [--json] -- <cmd…>` | start a pane (default: your shell) |
| `corral send <pane> <text> [--enter]` | type into a pane |
| `corral read <pane> [--lines N] [--scrollback] [--json]` | print a pane's screen as text |
| `corral wait <pane> --state blocked\|done\|idle\|working\|exited [--timeout S]` | wait for a state (exit code 2 on timeout) |
| `corral kill <pane> [--rm-worktree] [--delete-branch] [--force]` | close a pane |
| `corral rename <pane> <name>` | rename |
| `corral report --state S [--pane P]` | report state (hooks use this) |
| `corral integrate claude\|codex [--uninstall] [--yes]` | install agent hooks |
| `corral worktree ls [--json]` / `corral worktree rm <pane\|path> [--force] [--delete-branch]` | worktrees |
| `corral diff <pane> [--stat] [--json]` | what an agent changed in its worktree |
| `corral commit <pane> -m MSG` / `corral merge <pane>` / `corral discard <pane> --yes` | land or drop an agent's work |
| `corral server [--daemon]`, `corral kill-server` | run / stop the server (stopping ends every agent) |
| `corral debug dump <pane> [-o FILE]` | save a pane's screen as replayable bytes (for rule fixtures) |

Example: run an agent unattended and collect its answer.

```sh
p=$(corral new --worktree -- claude)
corral send "$p" "add input validation to the signup form" --enter
corral wait "$p" --state blocked --timeout 600 && corral read "$p" --lines 30
```

Each pane gets these environment variables:
- `CORRAL_PANE`: the pane's id
- `CORRAL_SOCKET`: the socket of the server it belongs to
- `CORRAL_BIN`: the corral binary

## Configuration

`~/.config/corral/config.toml` (all optional):

```toml
prefix = "C-b"                         # C-a … C-z or C-space
default_command = ""                   # run in each new shell, e.g. "claude"; you get the shell back when it exits
notifications = true
worktree_root = "~/.corral/worktrees"
sidebar_width = 30
sidebar_collapsed = false              # just the agent numbers (« / ^b c)
theme = "ranch"                        # ranch, ink or terminal
sidebar_order = "need"                 # need or project
```

Themes:

- **ranch** (default): warm field colours. Hay for corral's own marks, rust
  for blocked agents, sage for finished work, creek blue for work in
  progress, heather for branches. It draws on your terminal's background.
- **ink**: greys, with one amber signal kept for agents that need you.
- **terminal**: your terminal's own 16 colours, so it matches whatever
  palette you already use.

Environment variables:

| Variable | Default |
|---|---|
| `CORRAL_SOCKET` | `$TMPDIR/corral-$UID/default.sock` |
| `CORRAL_HOME` | `~/.corral` (logs, worktrees) |
| `CORRAL_CONFIG` | the config file path above |
| `CORRAL_LOG` | log filter, e.g. `debug` |

The server logs to `~/.corral/log/server.log`.

## How it works

```
corral (UI) ─────────┐
corral ls/send/read ─┼── Unix socket ($TMPDIR/corral-$UID/default.sock, 0700)
agent hooks (report) ┘          │
                         corral server (background, own session)
                         └─ pane = PTY + child process + vt100 emulator (10k lines scrollback)
                                   + state detector
```

- The server owns every PTY, so agents survive clients coming and going.
- Each pane's output feeds a terminal emulator (vt100) on the server.
- A client tells the server which agents it has on screen and at what size.
  For each one it gets a snapshot (scrollback included) and then the live
  byte stream, rendered with Ratatui and tui-term.
- Each PTY's size follows whichever client used it most recently, like tmux's
  `latest` policy.
- Keystrokes pass through to the pane untouched, apart from the prefix key,
  mouse events and bracketed pastes (which pass through whole).
- The server answers terminal queries (cursor position, device attributes),
  so programs that ask the terminal about itself start normally.
- Messages are MessagePack in length-delimited frames, with a strict
  protocol-version handshake.

## Compared to

**tmux / screen** keep processes alive and attachable too, but know nothing
about agents. You have to check each window for blocked prompts yourself, and
setting up worktrees is up to you.

**[Herdr](https://herdr.dev)** is the established tool in this space and the
inspiration for corral's server design: a server that owns the terminals,
attachable clients, agent state detection and a CLI API. It is far more
mature, with features corral doesn't have, such as tabs, plugins, session
restore after a reboot and access from other machines. corral focuses on the
loop of running agents in parallel and landing their work:

| | Herdr | corral |
|---|---|---|
| Sidebar | workspaces and agents | agents sorted by what they need from you, with waiting timers |
| Worktrees | create, open and remove worktree workspaces | one per agent on request, with change counts in the sidebar |
| Agent's changes | left to other tools | diff review in the UI, with commit, merge and discard |

## Limitations (v0.1)

- macOS and Linux only.
- At most four agents on screen at once, in fixed layouts (no free-form
  splits or tabs).
- Agents don't survive a reboot or `corral kill-server`.
- Programs inside panes don't receive mouse events; the wheel scrolls
  corral's scrollback instead.
- Diff review merges with a merge commit only (no squash or rebase yet), and
  conflicts must be resolved outside corral.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test --test tui -- --ignored --nocapture   # renders a real `claude` pane (needs claude)
```

The integration tests run the real binary as a server on a private socket,
and drive the TUI inside a PTY. To look at the UI from a script, the `drive`
example runs any command in a PTY and prints its screen:

```sh
cargo run --example drive -- 120x32 'wait:corral' 'type:\x02?' 'wait:help' dump -- corral
```

## License

MIT © 2026 bottercode
