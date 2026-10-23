//! Selecting text with the mouse. corral reads the mouse itself (clicks,
//! the wheel), which turns off the terminal's own drag-to-select, so it
//! selects for you instead: drag inside a pane to highlight text in that
//! pane only, let go to copy it. The copy goes to the clipboard through the
//! terminal (OSC 52, which also works over SSH) and, on this machine,
//! through `pbcopy`, `wl-copy` or `xclip` when one is there.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::protocol::PaneId;

/// A selection in one pane, in its visible cells (row, column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub pane: PaneId,
    /// Where the drag started.
    pub anchor: (u16, u16),
    /// Where the mouse is now.
    pub head: (u16, u16),
    /// The button is still down.
    pub dragging: bool,
}

impl Selection {
    pub fn new(pane: PaneId, at: (u16, u16)) -> Selection {
        Selection {
            pane,
            anchor: at,
            head: at,
            dragging: true,
        }
    }

    /// Start and end, in reading order.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Nothing selected yet (a click, not a drag).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Whether the cell is selected: whole lines between the first and
    /// last, as text runs.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let ((r0, c0), (r1, c1)) = self.ordered();
        (row, col) >= (r0, c0) && (row, col) <= (r1, c1) && r0 <= row && row <= r1
    }

    /// The selected text, without the spaces that pad each line.
    pub fn text(&self, screen: &vt100::Screen) -> String {
        let ((r0, c0), (r1, c1)) = self.ordered();
        let raw = screen.contents_between(r0, c0, r1, c1 + 1);
        let lines: Vec<&str> = raw.split('\n').map(str::trim_end).collect();
        lines.join("\n")
    }
}

/// OSC 52: asks the terminal to put `text` on the clipboard.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Copy with the clipboard tool of this machine, when corral runs on the
/// machine in front of you (not over SSH). True if one took it.
pub fn copy_locally(text: &str) -> bool {
    if cfg!(test) || std::env::var_os("SSH_CONNECTION").is_some() {
        return false;
    }
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    tools.iter().any(|(tool, args)| {
        let Ok(mut child) = Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
        child.wait().is_ok_and(|s| s.success()) && wrote
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo\n".as_bytes()), "aMOpbGxvCg==");
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
    }

    #[test]
    fn selects_runs_of_text_in_either_direction() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"first line   \r\nsecond line\r\nthird");
        // Dragged backwards, from "line" on row 1 to "line" on row 0.
        let mut s = Selection::new("p1".into(), (1, 10));
        s.head = (0, 6);
        assert_eq!(s.text(p.screen()), "line\nsecond line");
        assert!(s.contains(0, 6) && s.contains(0, 19) && s.contains(1, 0) && s.contains(1, 10));
        assert!(!s.contains(0, 5) && !s.contains(1, 11) && !s.contains(2, 0));
        // Within one line.
        s.anchor = (2, 0);
        s.head = (2, 2);
        assert_eq!(s.text(p.screen()), "thi");
        assert!(!s.is_empty());
        s.head = s.anchor;
        assert!(s.is_empty());
    }
}
