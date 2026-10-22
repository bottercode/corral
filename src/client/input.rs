//! Raw stdin handling for the TUI.
//!
//! [`Splitter`] cuts the byte stream into: bytes to pass through to the
//! focused pane, the prefix key, SGR mouse events (corral turns mouse
//! reporting on for the sidebar and the wheel), and bracketed pastes, which
//! always pass through untouched. [`decode_keys`] turns bytes into keys for
//! corral's own dialogs and prefix commands.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    LeftDown,
    LeftUp,
    WheelUp,
    WheelDown,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    /// 0-based column and row.
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// Bytes for the focused pane.
    Bytes(Vec<u8>),
    /// The prefix key was pressed.
    Prefix,
    Mouse(Mouse),
    /// A bracketed paste, markers included.
    Paste(Vec<u8>),
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

pub struct Splitter {
    prefix: u8,
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

enum Seq {
    Mouse(Mouse, usize),
    Invalid,
    Incomplete,
}

/// Parse `ESC [ < b ; x ; y (M|m)` at the start of `data`.
fn parse_sgr_mouse(data: &[u8]) -> Seq {
    let body = &data[3..];
    let mut nums = [0u32; 3];
    let mut idx = 0;
    let mut digits = 0;
    for (i, &c) in body.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                if digits > 5 {
                    return Seq::Invalid;
                }
                nums[idx] = nums[idx] * 10 + u32::from(c - b'0');
                digits += 1;
            }
            b';' if idx < 2 && digits > 0 => {
                idx += 1;
                digits = 0;
            }
            b'M' | b'm' if idx == 2 && digits > 0 => {
                let [b, x, y] = nums;
                let release = c == b'm';
                let kind = match (b & !(4 | 8 | 16), release) {
                    (0, false) => MouseKind::LeftDown,
                    (0, true) => MouseKind::LeftUp,
                    (64, _) => MouseKind::WheelUp,
                    (65, _) => MouseKind::WheelDown,
                    _ => MouseKind::Other,
                };
                let m = Mouse {
                    kind,
                    x: x.saturating_sub(1).min(u32::from(u16::MAX)) as u16,
                    y: y.saturating_sub(1).min(u32::from(u16::MAX)) as u16,
                };
                return Seq::Mouse(m, 3 + i + 1);
            }
            _ => return Seq::Invalid,
        }
    }
    Seq::Incomplete
}

impl Splitter {
    pub fn new(prefix: u8) -> Self {
        Splitter {
            prefix,
            pending: Vec::new(),
            paste: None,
        }
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Chunk> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(input);
        let mut out = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        let flush = |bytes: &mut Vec<u8>, out: &mut Vec<Chunk>| {
            if !bytes.is_empty() {
                out.push(Chunk::Bytes(std::mem::take(bytes)));
            }
        };

        let mut i = 0;
        while i < data.len() {
            if let Some(buf) = self.paste.as_mut() {
                let rest = &data[i..];
                match find(rest, PASTE_END) {
                    Some(at) => {
                        buf.extend_from_slice(&rest[..at + PASTE_END.len()]);
                        out.push(Chunk::Paste(self.paste.take().unwrap_or_default()));
                        i += at + PASTE_END.len();
                    }
                    None => {
                        // Keep a possible partial end marker for next time.
                        let keep = partial_suffix(rest, PASTE_END);
                        buf.extend_from_slice(&rest[..rest.len() - keep]);
                        self.pending = rest[rest.len() - keep..].to_vec();
                        i = data.len();
                    }
                }
                continue;
            }

            let b = data[i];
            if b == self.prefix {
                flush(&mut bytes, &mut out);
                out.push(Chunk::Prefix);
                i += 1;
                continue;
            }
            if b == 0x1b {
                let rest = &data[i..];
                if rest.starts_with(b"\x1b[<") {
                    match parse_sgr_mouse(rest) {
                        Seq::Mouse(m, len) => {
                            flush(&mut bytes, &mut out);
                            out.push(Chunk::Mouse(m));
                            i += len;
                            continue;
                        }
                        Seq::Incomplete => {
                            flush(&mut bytes, &mut out);
                            self.pending = rest.to_vec();
                            break;
                        }
                        Seq::Invalid => {}
                    }
                } else if rest.starts_with(PASTE_START) {
                    flush(&mut bytes, &mut out);
                    self.paste = Some(PASTE_START.to_vec());
                    i += PASTE_START.len();
                    continue;
                } else if rest.len() >= 3
                    && rest.len() < PASTE_START.len()
                    && PASTE_START.starts_with(rest)
                {
                    // Possibly a paste marker split across reads.
                    flush(&mut bytes, &mut out);
                    self.pending = rest.to_vec();
                    break;
                }
            }
            bytes.push(b);
            i += 1;
        }
        flush(&mut bytes, &mut out);
        out
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `data` that is a proper prefix of `marker`.
fn partial_suffix(data: &[u8], marker: &[u8]) -> usize {
    (1..marker.len())
        .rev()
        .find(|&n| n <= data.len() && data.ends_with(&marker[..n]))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Unknown,
}

/// Decode raw terminal input into keys.
pub fn decode_keys(data: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        match b {
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                i += 1;
            }
            b'\t' => {
                keys.push(Key::Tab);
                i += 1;
            }
            0x7f | 0x08 => {
                keys.push(Key::Backspace);
                i += 1;
            }
            0x1b => {
                let (k, len) = decode_escape(&data[i..]);
                keys.push(k);
                i += len;
            }
            0x00 => {
                keys.push(Key::Ctrl(' '));
                i += 1;
            }
            1..=26 => {
                keys.push(Key::Ctrl((b'a' + b - 1) as char));
                i += 1;
            }
            0x1c..=0x1f => {
                keys.push(Key::Unknown);
                i += 1;
            }
            _ => {
                let len = utf8_len(b);
                match data
                    .get(i..i + len)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| s.chars().next())
                {
                    Some(c) => {
                        keys.push(Key::Char(c));
                        i += len;
                    }
                    None => {
                        keys.push(Key::Unknown);
                        i += 1;
                    }
                }
            }
        }
    }
    keys
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn decode_escape(data: &[u8]) -> (Key, usize) {
    match data.get(1) {
        None => (Key::Esc, 1),
        Some(b'[') => {
            // CSI: parameters then a final byte in 0x40..=0x7e.
            let Some(end) = data[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
                return (Key::Unknown, data.len());
            };
            let params = &data[2..2 + end];
            let fin = data[2 + end];
            let len = 3 + end;
            let key = match (params, fin) {
                (_, b'A') => Key::Up,
                (_, b'B') => Key::Down,
                (_, b'C') => Key::Right,
                (_, b'D') => Key::Left,
                (_, b'H') => Key::Home,
                (_, b'F') => Key::End,
                (_, b'Z') => Key::BackTab,
                (b"1" | b"7", b'~') => Key::Home,
                (b"4" | b"8", b'~') => Key::End,
                (b"3", b'~') => Key::Delete,
                (b"5", b'~') => Key::PageUp,
                (b"6", b'~') => Key::PageDown,
                _ => Key::Unknown,
            };
            (key, len)
        }
        Some(b'O') => match data.get(2) {
            Some(b'A') => (Key::Up, 3),
            Some(b'B') => (Key::Down, 3),
            Some(b'C') => (Key::Right, 3),
            Some(b'D') => (Key::Left, 3),
            Some(b'H') => (Key::Home, 3),
            Some(b'F') => (Key::End, 3),
            Some(_) => (Key::Unknown, 3),
            None => (Key::Unknown, 2),
        },
        // Alt+key or a lone Esc followed by typing: report Esc, then the key.
        Some(_) => (Key::Esc, 1),
    }
}

/// What a wheel notch over a pane means to the program in it: a mouse
/// report when it asked for the mouse, `lines` arrow keys on the alternate
/// screen (as terminals do for full-screen programs, which have no
/// scrollback), or `None` to scroll corral's own scrollback. `col` and
/// `row` are 0-based within the pane.
pub fn wheel_for_pane(
    screen: &vt100::Screen,
    up: bool,
    col: u16,
    row: u16,
    lines: usize,
) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};
    let button: u32 = if up { 64 } else { 65 };
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if screen.mouse_protocol_mode() != MouseProtocolMode::None {
        return Some(match screen.mouse_protocol_encoding() {
            MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{x};{y}M").into_bytes(),
            MouseProtocolEncoding::Utf8 => {
                let mut out = b"\x1b[M".to_vec();
                for v in [button, x, y] {
                    let c = char::from_u32(32 + v).unwrap_or(' ');
                    out.extend(c.to_string().as_bytes());
                }
                out
            }
            // One byte each: positions past 223 can't be told apart.
            MouseProtocolEncoding::Default => {
                let byte = |v: u32| (32 + v).min(255) as u8;
                vec![0x1b, b'[', b'M', byte(button), byte(x), byte(y)]
            }
        });
    }
    if screen.alternate_screen() {
        let arrow: &[u8] = match (up, screen.application_cursor()) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(lines));
    }
    None
}

/// Adjust bytes bound for a pane to the modes its program asked for: strip
/// paste markers it didn't enable, and use SS3 arrows in application
/// cursor mode.
pub fn adapt_for_pane(
    bytes: &[u8],
    bracketed_paste: bool,
    app_cursor: bool,
    is_paste: bool,
) -> Vec<u8> {
    if is_paste {
        if bracketed_paste {
            return bytes.to_vec();
        }
        let inner = bytes.strip_prefix(PASTE_START).unwrap_or(bytes);
        let inner = inner.strip_suffix(PASTE_END).unwrap_or(inner);
        return inner.to_vec();
    }
    if !app_cursor {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b
            && bytes.get(i + 1) == Some(&b'[')
            && let Some(&f) = bytes.get(i + 2)
            && matches!(f, b'A' | b'B' | b'C' | b'D' | b'H' | b'F')
        {
            out.extend_from_slice(&[0x1b, b'O', f]);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u8 = 0x02;

    #[test]
    fn plain_bytes_pass_through() {
        let mut s = Splitter::new(P);
        assert_eq!(s.feed(b"hello\r"), vec![Chunk::Bytes(b"hello\r".to_vec())]);
        let utf = "✳ héllo".as_bytes();
        assert_eq!(s.feed(utf), vec![Chunk::Bytes(utf.to_vec())]);
        // Other escape sequences (arrows, function keys) too.
        assert_eq!(
            s.feed(b"\x1b[A\x1bOP\x1b"),
            vec![Chunk::Bytes(b"\x1b[A\x1bOP\x1b".to_vec())]
        );
    }

    #[test]
    fn prefix_is_detected() {
        let mut s = Splitter::new(P);
        assert_eq!(
            s.feed(b"ab\x02nc"),
            vec![
                Chunk::Bytes(b"ab".to_vec()),
                Chunk::Prefix,
                Chunk::Bytes(b"nc".to_vec())
            ]
        );
        let mut s = Splitter::new(0x01);
        assert_eq!(s.feed(b"\x02"), vec![Chunk::Bytes(vec![0x02])]);
        assert_eq!(s.feed(b"\x01"), vec![Chunk::Prefix]);
    }

    #[test]
    fn sgr_mouse() {
        let mut s = Splitter::new(P);
        assert_eq!(
            s.feed(b"\x1b[<0;5;10M"),
            vec![Chunk::Mouse(Mouse {
                kind: MouseKind::LeftDown,
                x: 4,
                y: 9
            })]
        );
        assert_eq!(
            s.feed(b"\x1b[<0;5;10m\x1b[<64;1;1M\x1b[<65;1;1M"),
            vec![
                Chunk::Mouse(Mouse {
                    kind: MouseKind::LeftUp,
                    x: 4,
                    y: 9
                }),
                Chunk::Mouse(Mouse {
                    kind: MouseKind::WheelUp,
                    x: 0,
                    y: 0
                }),
                Chunk::Mouse(Mouse {
                    kind: MouseKind::WheelDown,
                    x: 0,
                    y: 0
                }),
            ]
        );
        // Ctrl-click still counts as a left click.
        assert_eq!(
            s.feed(b"\x1b[<16;2;3M"),
            vec![Chunk::Mouse(Mouse {
                kind: MouseKind::LeftDown,
                x: 1,
                y: 2
            })]
        );
    }

    #[test]
    fn mouse_split_across_reads() {
        let mut s = Splitter::new(P);
        assert_eq!(s.feed(b"x\x1b[<0;12"), vec![Chunk::Bytes(b"x".to_vec())]);
        assert_eq!(
            s.feed(b";7M"),
            vec![Chunk::Mouse(Mouse {
                kind: MouseKind::LeftDown,
                x: 11,
                y: 6
            })]
        );
    }

    #[test]
    fn malformed_mouse_is_passed_through() {
        let mut s = Splitter::new(P);
        assert_eq!(s.feed(b"\x1b[<x"), vec![Chunk::Bytes(b"\x1b[<x".to_vec())]);
    }

    #[test]
    fn paste_passes_through_untouched() {
        let mut s = Splitter::new(P);
        // The prefix byte and a mouse-looking sequence inside a paste are data.
        let paste = b"\x1b[200~a\x02b\x1b[<0;1;1M\nc\x1b[201~";
        assert_eq!(
            s.feed(&[b"k".as_slice(), paste, b"z"].concat()),
            vec![
                Chunk::Bytes(b"k".to_vec()),
                Chunk::Paste(paste.to_vec()),
                Chunk::Bytes(b"z".to_vec()),
            ]
        );
    }

    #[test]
    fn paste_split_across_reads() {
        let mut s = Splitter::new(P);
        assert_eq!(s.feed(b"\x1b[20"), vec![]);
        assert_eq!(s.feed(b"0~hello \x02"), vec![]);
        assert_eq!(s.feed(b"world\x1b[2"), vec![]);
        assert_eq!(
            s.feed(b"01~"),
            vec![Chunk::Paste(b"\x1b[200~hello \x02world\x1b[201~".to_vec())]
        );
    }

    #[test]
    fn keys() {
        use Key::*;
        assert_eq!(
            decode_keys(b"a\r\t\x7f\x1b[A\x1bOB\x1b[5~\x1b[3~\x1b[Z\x15\x1b"),
            vec![
                Char('a'),
                Enter,
                Tab,
                Backspace,
                Up,
                Down,
                PageUp,
                Delete,
                BackTab,
                Ctrl('u'),
                Esc
            ]
        );
        assert_eq!(decode_keys("é✳".as_bytes()), vec![Char('é'), Char('✳')]);
        assert_eq!(decode_keys(b"\x1bx"), vec![Esc, Char('x')]);
    }

    #[test]
    fn adapting_input_to_pane_modes() {
        let paste = b"\x1b[200~hi\x1b[201~";
        assert_eq!(adapt_for_pane(paste, true, false, true), paste.to_vec());
        assert_eq!(adapt_for_pane(paste, false, false, true), b"hi".to_vec());
        assert_eq!(
            adapt_for_pane(b"\x1b[Ax\x1b[D", false, true, false),
            b"\x1bOAx\x1bOD".to_vec()
        );
        assert_eq!(
            adapt_for_pane(b"\x1b[A", false, false, false),
            b"\x1b[A".to_vec()
        );
    }

    fn wheel(setup: &[u8], up: bool) -> Option<Vec<u8>> {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(setup);
        wheel_for_pane(p.screen(), up, 4, 9, 3)
    }

    #[test]
    fn wheel_goes_where_the_program_wants_it() {
        // Plain shell: corral scrolls its own scrollback.
        assert_eq!(wheel(b"", true), None);
        // Full-screen program: arrow keys, in its cursor mode.
        assert_eq!(
            wheel(b"\x1b[?1049h", true),
            Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        assert_eq!(
            wheel(b"\x1b[?1049h\x1b[?1h", false),
            Some(b"\x1bOB\x1bOB\x1bOB".to_vec())
        );
        // Program that reads the mouse: a wheel report at the pane position.
        assert_eq!(
            wheel(b"\x1b[?1000h\x1b[?1006h", true),
            Some(b"\x1b[<64;5;10M".to_vec())
        );
        assert_eq!(
            wheel(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h", false),
            Some(b"\x1b[<65;5;10M".to_vec())
        );
        assert_eq!(
            wheel(b"\x1b[?1000h", true),
            Some(vec![0x1b, b'[', b'M', 96, 37, 42])
        );
    }
}
