//! Day-0 spike: run a command in a PTY, feed vt100, render with tui-term,
//! and resolve the foreground process via process_group_leader + sysinfo.
//!
//! cargo run --example spike -- claude

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::{Terminal, backend::TestBackend};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tui_term::widget::PseudoTerminal;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (prog, rest) = args.split_first().expect("usage: spike <cmd> [args]");

    let pty = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
    let mut cmd = CommandBuilder::new(prog);
    cmd.args(rest);
    cmd.cwd(std::env::temp_dir());
    cmd.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 10_000)));
    let mut reader = pty.master.try_clone_reader()?;
    let p = parser.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            p.lock().unwrap().process(&buf[..n]);
        }
    });

    std::thread::sleep(Duration::from_secs(4));

    let leader = pty.master.process_group_leader();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::everything());
    if let Some(pid) = leader {
        let proc_ = sys.process(Pid::from_u32(pid as u32));
        println!(
            "leader pid={pid} name={:?} cmd={:?}",
            proc_.map(|p| p.name().to_string_lossy().into_owned()),
            proc_.map(|p| p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>())
        );
    } else {
        println!("leader: none");
    }

    // Resize the PTY and the parser together, as the server will.
    pty.master.resize(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })?;
    parser.lock().unwrap().screen_mut().set_size(30, 100);
    std::thread::sleep(Duration::from_millis(800));

    let mut term = Terminal::new(TestBackend::new(100, 30))?;
    {
        let guard = parser.lock().unwrap();
        term.draw(|f| f.render_widget(PseudoTerminal::new(guard.screen()), f.area()))?;
        let snap = guard.screen().state_formatted();
        println!("snapshot bytes: {}", snap.len() + guard.screen().contents_formatted().len());
    }
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let line = line.trim_end();
        if !line.is_empty() {
            println!("|{line}");
        }
    }

    child.kill().ok();
    child.wait().ok();
    Ok(())
}
