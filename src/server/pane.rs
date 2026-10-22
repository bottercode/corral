//! A pane: one child process in a PTY, the server's terminal emulator for
//! it, and its agent state.

use crate::detect::agents::Running;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Context, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::detect::Detector;
use crate::paths;
use crate::protocol::{PaneId, PaneInfo, WorktreeInfo};

pub const SCROLLBACK: usize = 10_000;

/// vt100 callbacks: answers terminal queries the way a real terminal would
/// (programs like crossterm-based TUIs wait for these replies) and records
/// the window title.
#[derive(Default)]
pub struct Responder {
    pub replies: Vec<u8>,
    pub title: Option<String>,
}

impl vt100::Callbacks for Responder {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            // DSR: cursor position report.
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // DSR: status report → OK.
            (None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            // Primary device attributes: VT102-ish with advanced video.
            (None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            // Secondary device attributes.
            (Some(b'>'), 'c', 0) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let t = String::from_utf8_lossy(title).trim().to_string();
        self.title = (!t.is_empty()).then_some(t);
    }
}

pub type Parser = vt100::Parser<Responder>;

pub fn new_parser(rows: u16, cols: u16) -> Parser {
    vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Responder::default())
}

pub struct SpawnSpec {
    pub id: PaneId,
    pub name: String,
    pub named: bool,
    pub workspace: String,
    /// argv to run; empty means the user's login shell.
    pub argv: Vec<String>,
    /// What to show as the pane's command.
    pub display: Vec<String>,
    pub cwd: PathBuf,
    pub origin: PathBuf,
    pub socket: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub worktree: Option<WorktreeInfo>,
}

/// Variables that describe the session that started the server rather
/// than user configuration. Agent settings such as `CLAUDE_CODE_USE_BEDROCK`
/// or `ANTHROPIC_API_KEY` are kept.
pub fn is_inherited_session_var(k: &str) -> bool {
    const EXACT: &[&str] = &[
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TERM_SESSION_ID",
        "TMUX",
        "TMUX_PANE",
        "CLAUDECODE",
        "CLAUDE_PID",
        "CLAUDE_EFFORT",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS",
        "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
        "CODEX_SANDBOX",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_THREAD_ID",
    ];
    const PREFIXES: &[&str] = &["CLAUDE_CODE_SESSION_", "CLAUDE_CODE_MESSAGING_"];
    EXACT.contains(&k) || PREFIXES.iter().any(|p| k.starts_with(p))
}

pub struct Pane {
    pub id: PaneId,
    pub name: String,
    /// The user chose the name; don't replace it with the agent name.
    pub named: bool,
    pub workspace: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// Where the pane was started from (the repo, for worktree panes).
    pub origin: PathBuf,
    pub parser: Parser,
    /// Number of output chunks processed so far.
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pub pid: Option<u32>,
    pub detector: Detector,
    /// Screen changed since the rules last looked at it.
    pub dirty: bool,
    pub agent: Option<String>,
    /// The agent process running now (the last one identified), if any.
    pub running: Option<Running>,
    /// The session id the agent's hook last reported, and when (Unix
    /// seconds).
    pub hook_session: Option<(String, u64)>,
    pub worktree: Option<WorktreeInfo>,
    pub exit_code: Option<i32>,
    pub exited: bool,
}

pub struct Spawned {
    pub pane: Pane,
    pub reader: Box<dyn Read + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

/// Before the screen loses rows, scroll so the cursor's line stays in view.
/// Terminals do this, pushing lines off the top into scrollback; vt100 cuts
/// rows off the bottom instead, which would hide the latest output (and the
/// prompt) of a pane shrunk after it finished writing. Full-screen programs
/// on the alternate screen redraw themselves, so they're left alone.
pub fn keep_cursor_in_view<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>, rows: u16) {
    let screen = parser.screen();
    let (old_rows, _) = screen.size();
    let (row, col) = screen.cursor_position();
    if screen.alternate_screen() || rows >= old_rows || row < rows {
        return;
    }
    let up = row + 1 - rows;
    // To the bottom row, line feeds to scroll, then back to the cursor's
    // new place.
    let mut seq = format!("\x1b[{old_rows};1H");
    seq.push_str(&"\n".repeat(up as usize));
    seq.push_str(&format!("\x1b[{};{}H", row - up + 1, col + 1));
    parser.process(seq.as_bytes());
}

pub fn spawn(spec: SpawnSpec) -> Result<Spawned> {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a pty")?;

    let mut cmd = if spec.argv.is_empty() {
        CommandBuilder::new_default_prog()
    } else {
        let mut c = CommandBuilder::new(&spec.argv[0]);
        c.args(&spec.argv[1..]);
        c
    };
    cmd.cwd(&spec.cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env(paths::PANE_ENV, &spec.id);
    cmd.env(paths::SOCKET_ENV, &spec.socket);
    if let Ok(exe) = std::env::current_exe() {
        cmd.env(paths::BIN_ENV, exe);
    }
    // A pane is a fresh terminal: it shouldn't inherit the identity of the
    // terminal or agent session that happened to start the server.
    for (key, _) in std::env::vars_os() {
        if let Some(k) = key.to_str()
            && is_inherited_session_var(k)
        {
            cmd.env_remove(k);
        }
    }

    let child = pty.slave.spawn_command(cmd).with_context(|| {
        format!(
            "starting {:?} in {}",
            spec.display.join(" "),
            spec.cwd.display()
        )
    })?;
    drop(pty.slave);

    let reader = pty
        .master
        .try_clone_reader()
        .context("cloning pty reader")?;
    let mut writer = pty.master.take_writer().context("taking pty writer")?;
    let (input, rx) = mpsc::channel::<Vec<u8>>();
    // Writes go through a thread so an agent that stops reading its input
    // can never stall the server.
    std::thread::Builder::new()
        .name(format!("pty-writer-{}", spec.id))
        .spawn(move || {
            for buf in rx {
                if writer.write_all(&buf).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })
        .context("spawning writer thread")?;

    let pane = Pane {
        id: spec.id,
        name: spec.name,
        named: spec.named,
        workspace: spec.workspace,
        command: spec.display,
        cwd: spec.cwd,
        origin: spec.origin,
        parser: new_parser(spec.rows, spec.cols),
        seq: 0,
        cols: spec.cols,
        rows: spec.rows,
        killer: child.clone_killer(),
        pid: child.process_id(),
        master: pty.master,
        input,
        detector: Detector::new(),
        dirty: false,
        agent: None,
        running: None,
        hook_session: None,
        worktree: spec.worktree,
        exit_code: None,
        exited: false,
    };
    Ok(Spawned {
        pane,
        reader,
        child,
    })
}

impl Pane {
    pub fn info(&self) -> PaneInfo {
        PaneInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            workspace: self.workspace.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            agent: self.agent.clone(),
            state: self.detector.state(),
            pid: self.pid,
            exit_code: self.exit_code,
            title: self.parser.callbacks().title.clone(),
            worktree: self.worktree.clone(),
            cols: self.cols,
            rows: self.rows,
            state_secs: self.detector.state_age(std::time::Instant::now()).as_secs(),
        }
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.input.send(bytes);
        }
    }

    /// Feed output to the emulator, answering any terminal queries in it.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let replies = std::mem::take(&mut self.parser.callbacks_mut().replies);
        if !self.exited {
            self.write(replies);
        }
        self.seq += 1;
        self.dirty = true;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        keep_cursor_in_view(&mut self.parser, rows);
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        self.dirty = true;
        true
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }

    /// The foreground process of the PTY (falls back to the child).
    pub fn foreground_pid(&self) -> Option<u32> {
        self.master
            .process_group_leader()
            .and_then(|p| u32::try_from(p).ok())
            .or(self.pid)
    }

    pub fn snapshot(&mut self) -> Vec<u8> {
        snapshot(self.parser.screen_mut())
    }

    pub fn text(&mut self, lines: Option<usize>, scrollback: bool) -> String {
        screen_text(self.parser.screen_mut(), lines, scrollback)
    }
}

/// History rows (oldest first) as `(formatted, plain)` pairs.
fn history_rows(screen: &mut vt100::Screen) -> Vec<(Vec<u8>, String)> {
    let (rows, cols) = screen.size();
    let mut out = Vec::new();
    if screen.alternate_screen() {
        return out;
    }
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();
    let mut off = total;
    while off > 0 {
        screen.set_scrollback(off);
        let n = off.min(rows as usize);
        let formatted = screen.rows_formatted(0, cols).take(n);
        let plain = screen.rows(0, cols).take(n);
        out.extend(formatted.zip(plain));
        off -= n;
    }
    screen.set_scrollback(0);
    out
}

/// Bytes that rebuild `screen` in a fresh parser of the same size:
/// scrollback history first, then the visible screen and input modes.
pub fn snapshot(screen: &mut vt100::Screen) -> Vec<u8> {
    let (rows, _) = screen.size();
    let history = history_rows(screen);
    let mut out = Vec::new();
    if !history.is_empty() {
        for (row, _) in &history {
            out.extend_from_slice(b"\x1b[m");
            out.extend_from_slice(row);
            out.extend_from_slice(b"\r\n");
        }
        // Scroll the last history rows off the visible screen.
        for _ in 1..rows {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\x1b[m");
    }
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(&screen.state_formatted());
    out
}

/// Plain text of the screen (and optionally history), trailing blank lines
/// removed, limited to the last `lines` lines.
pub fn screen_text(screen: &mut vt100::Screen, lines: Option<usize>, scrollback: bool) -> String {
    let (_, cols) = screen.size();
    let mut all: Vec<String> = Vec::new();
    if scrollback {
        all.extend(history_rows(screen).into_iter().map(|(_, p)| p));
    }
    all.extend(screen.rows(0, cols));
    let mut all: Vec<String> = all.into_iter().map(|l| l.trim_end().to_string()).collect();
    while all.last().is_some_and(|l| l.is_empty()) {
        all.pop();
    }
    let start = lines.map_or(0, |n| all.len().saturating_sub(n));
    let mut s = all[start..].join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(p: &vt100::Parser) -> String {
        p.screen().contents()
    }

    #[test]
    fn shrinking_keeps_the_latest_lines() {
        let mut p = vt100::Parser::new(24, 80, SCROLLBACK);
        for i in 0..100 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        p.process(b"$ ");
        keep_cursor_in_view(&mut p, 10);
        p.screen_mut().set_size(10, 60);
        let t = text(&p);
        assert!(t.contains("line99") && t.trim_end().ends_with('$'), "{t}");
        assert_eq!(p.screen().cursor_position(), (9, 2));
        // What scrolled off is in the scrollback.
        p.screen_mut().set_scrollback(usize::MAX);
        assert!(p.screen().scrollback() >= 90);

        // Room to spare, or growing: nothing moves.
        let mut p = vt100::Parser::new(24, 80, SCROLLBACK);
        p.process(b"one\r\ntwo");
        keep_cursor_in_view(&mut p, 10);
        p.screen_mut().set_size(10, 80);
        assert!(text(&p).starts_with("one\ntwo"));
        assert_eq!(p.screen().cursor_position(), (1, 3));
    }

    fn feed(p: &mut Parser, s: &str) {
        p.process(s.as_bytes());
    }

    #[test]
    fn answers_cursor_position_and_device_attributes() {
        let mut p = new_parser(24, 80);
        feed(&mut p, "ab\r\ncd\x1b[6n\x1b[c\x1b[5n");
        let replies = std::mem::take(&mut p.callbacks_mut().replies);
        assert_eq!(replies, b"\x1b[2;3R\x1b[?1;2c\x1b[0n");
    }

    #[test]
    fn session_markers_are_dropped_but_config_is_kept() {
        for k in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_CHILD_SESSION",
            "TMUX",
        ] {
            assert!(is_inherited_session_var(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_USE_BEDROCK",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CONFIG_DIR",
            "PATH",
            "HOME",
        ] {
            assert!(!is_inherited_session_var(k), "{k}");
        }
    }

    #[test]
    fn records_title() {
        let mut p = new_parser(24, 80);
        feed(&mut p, "\x1b]0;\u{2733} Fix bug\x07");
        assert_eq!(p.callbacks().title.as_deref(), Some("\u{2733} Fix bug"));
    }

    #[test]
    fn snapshot_rebuilds_screen_and_history() {
        let mut p = new_parser(5, 20);
        for i in 0..40 {
            feed(&mut p, &format!("\x1b[3{}mline {i}\x1b[m\r\n", i % 7));
        }
        feed(&mut p, "\x1b[1mbold\x1b[m tail");
        let snap = snapshot(p.screen_mut());

        let mut q = vt100::Parser::new(5, 20, SCROLLBACK);
        q.process(&snap);
        assert_eq!(q.screen().contents(), p.screen().contents());
        assert_eq!(q.screen().cursor_position(), p.screen().cursor_position());
        assert_eq!(
            q.screen().cell(4, 0).unwrap().bold(),
            p.screen().cell(4, 0).unwrap().bold()
        );
        // Same history: the oldest line is reachable in both.
        let mut a = p.screen().clone();
        let mut b = q.screen().clone();
        assert_eq!(
            screen_text(&mut a, None, true),
            screen_text(&mut b, None, true)
        );
        assert!(screen_text(&mut b, None, true).starts_with("line 0\n"));
        // Colours survive in history too: "line 0" was printed with SGR 30.
        a.set_scrollback(usize::MAX);
        b.set_scrollback(usize::MAX);
        assert_eq!(a.scrollback(), b.scrollback());
        assert_eq!(b.cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(0));
        assert_eq!(
            b.cell(0, 0).unwrap().fgcolor(),
            a.cell(0, 0).unwrap().fgcolor()
        );
    }

    #[test]
    fn snapshot_of_alternate_screen() {
        let mut p = new_parser(5, 20);
        feed(&mut p, "shell\r\n\x1b[?1049h\x1b[Hfullscreen app");
        let snap = snapshot(p.screen_mut());
        let mut q = vt100::Parser::new(5, 20, SCROLLBACK);
        q.process(&snap);
        assert!(q.screen().alternate_screen());
        assert_eq!(q.screen().contents(), p.screen().contents());
    }

    #[test]
    fn text_lines() {
        let mut p = new_parser(5, 20);
        feed(&mut p, "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng");
        let s = p.screen_mut();
        assert_eq!(screen_text(s, None, false), "c\nd\ne\nf\ng\n");
        assert_eq!(screen_text(s, Some(2), false), "f\ng\n");
        assert_eq!(screen_text(s, None, true), "a\nb\nc\nd\ne\nf\ng\n");
        assert_eq!(s.scrollback(), 0);
    }
}
