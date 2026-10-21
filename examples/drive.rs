//! Drive a terminal program in a PTY from a script and print its screen.
//! Handy for checking the TUI by hand without a real terminal.
//!
//!   cargo run --example drive -- 120x32 'wait:corral' 'type:\x02j' 'sleep:500' dump -- corral
//!
//! Steps: `wait:TEXT` (up to 15 s), `gone:TEXT`, `type:BYTES` (\xNN, \r, \e
//! escapes), `sleep:MS`, `dump`. Everything after `--` is the command.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b'e' => out.push(0x1b),
                b't' => out.push(b'\t'),
                b'\\' => out.push(b'\\'),
                b'x' if i + 3 < b.len() + 1 => {
                    let hex = std::str::from_utf8(&b[i + 2..i + 4]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    i += 4;
                    continue;
                }
                c => out.push(c),
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args
        .iter()
        .position(|a| a == "--")
        .expect("missing -- before the command");
    let (spec, cmd) = (&args[..split], &args[split + 1..]);
    let (cols, rows) = spec[0].split_once('x').expect("size like 120x32");
    let (cols, rows): (u16, u16) = (cols.parse()?, rows.parse()?);

    let pty = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut c = CommandBuilder::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.env("TERM", "xterm-256color");
    if let Ok(dir) = std::env::current_dir() {
        c.cwd(dir);
    }
    let mut child = pty.slave.spawn_command(c)?;
    drop(pty.slave);
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
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
    let mut writer = pty.master.take_writer()?;
    let screen = || parser.lock().unwrap().screen().contents();

    for step in &spec[1..] {
        let (op, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match op {
            "wait" | "gone" => {
                let deadline = Instant::now() + Duration::from_secs(15);
                while screen().contains(arg) != (op == "wait") {
                    if Instant::now() > deadline {
                        println!("== TIMEOUT {step}\n{}", screen());
                        child.kill().ok();
                        std::process::exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                println!("== ok {step}");
            }
            "type" => {
                writer.write_all(&unescape(arg))?;
                writer.flush()?;
            }
            "sleep" => std::thread::sleep(Duration::from_millis(arg.parse()?)),
            "dump" => {
                println!("== screen");
                for line in screen().lines() {
                    println!("|{}", line.trim_end());
                }
            }
            other => anyhow::bail!("unknown step {other}"),
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    if let Ok(Some(status)) = child.try_wait() {
        println!("== exited {status:?}");
    } else {
        child.kill().ok();
        println!("== still running (killed)");
    }
    Ok(())
}
