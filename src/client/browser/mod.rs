//! The browser tile: a real web page next to the agents (`^b u`, or Enter
//! on a top story). corral runs the user's Chrome headless ([`cdp`]) and
//! draws its frames into the tile with kitty graphics ([`kitty`]), so it
//! needs a terminal that speaks that (Ghostty, kitty, WezTerm); elsewhere
//! links open in the system browser instead. Clicks, the wheel and typing
//! go to the page while the tile has the keyboard; `^b` keys still drive
//! corral.

pub mod cdp;
pub mod kitty;

use ratatui::layout::Rect;

use super::input::Key;
pub use cdp::Viewport;

/// The browser's place in the list of tiles (pane ids never start with `@`).
pub const TILE: &str = "@browser";

/// To the browser task.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserCmd {
    Navigate(String),
    Back,
    Forward,
    Reload,
    Resize(Viewport),
    /// CSS pixels.
    Click {
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        dy: f64,
    },
    Text(String),
    Key(KeyInput),
    Quit,
}

/// From the browser task.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserEvent {
    /// A new frame: a PNG, base64.
    Frame(String),
    Page {
        url: String,
        title: String,
    },
    Loading(bool),
    Failed(String),
}

/// One key for the page, as DevTools describes keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: String,
    pub code: String,
    pub vk: u32,
    pub text: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyInput {
    pub fn special(key: &str, code: &str, vk: u32, text: Option<&str>) -> KeyInput {
        KeyInput {
            key: key.into(),
            code: code.into(),
            vk,
            text: text.map(String::from),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn char(c: char, ctrl: bool) -> KeyInput {
        let upper = c.to_ascii_uppercase();
        let (code, vk) = if c.is_ascii_alphabetic() {
            (format!("Key{upper}"), upper as u32)
        } else if c.is_ascii_digit() {
            (format!("Digit{c}"), c as u32)
        } else if c == ' ' {
            ("Space".into(), 32)
        } else {
            (String::new(), 0)
        };
        KeyInput {
            key: c.to_string(),
            code,
            vk,
            text: (!ctrl).then(|| c.to_string()),
            ctrl,
            alt: false,
            shift: c.is_ascii_uppercase(),
        }
    }

    /// The page's version of a terminal key; `None` for keys it can't use.
    pub fn from_key(k: Key) -> Option<KeyInput> {
        let s = KeyInput::special;
        Some(match k {
            Key::Char(c) => KeyInput::char(c, false),
            Key::Ctrl(c) => KeyInput::char(c, true),
            Key::Enter => s("Enter", "Enter", 13, Some("\r")),
            Key::Tab => s("Tab", "Tab", 9, None),
            Key::BackTab => KeyInput {
                shift: true,
                ..s("Tab", "Tab", 9, None)
            },
            Key::Backspace => s("Backspace", "Backspace", 8, None),
            Key::Delete => s("Delete", "Delete", 46, None),
            Key::Esc => s("Escape", "Escape", 27, None),
            Key::Up => s("ArrowUp", "ArrowUp", 38, None),
            Key::Down => s("ArrowDown", "ArrowDown", 40, None),
            Key::Left => s("ArrowLeft", "ArrowLeft", 37, None),
            Key::Right => s("ArrowRight", "ArrowRight", 39, None),
            Key::Home => s("Home", "Home", 36, None),
            Key::End => s("End", "End", 35, None),
            Key::PageUp => s("PageUp", "PageUp", 33, None),
            Key::PageDown => s("PageDown", "PageDown", 34, None),
            Key::Unknown => return None,
        })
    }
}

/// What a click on the tile's bar does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarClick {
    Back,
    Forward,
    Reload,
    Address,
    Close,
}

/// The page as the client knows it.
#[derive(Debug, Default)]
pub struct Browser {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub error: Option<String>,
    /// The browser stopped (it failed or quit); the next page starts it
    /// again.
    pub dead: bool,
    /// The latest frame (PNG, base64) and its number.
    pub frame: Option<String>,
    pub frame_seq: u64,
    /// The size last asked of the page.
    pub viewport: Option<Viewport>,
    /// Where the bar's buttons were drawn.
    pub hits: Vec<(Rect, BarClick)>,
}

impl Browser {
    pub fn on_event(&mut self, ev: BrowserEvent) {
        match ev {
            BrowserEvent::Frame(png) => {
                self.frame = Some(png);
                self.frame_seq += 1;
                self.error = None;
            }
            BrowserEvent::Page { url, title } => {
                self.url = url;
                self.title = title;
            }
            BrowserEvent::Loading(l) => self.loading = l,
            BrowserEvent::Failed(e) => {
                self.error = Some(e);
                self.loading = false;
                self.dead = true;
            }
        }
    }

    pub fn click_at(&self, x: u16, y: u16) -> Option<BarClick> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains((x, y).into()))
            .map(|&(_, c)| c)
    }
}

/// How big the page is for a tile body of `cells`, given the size of one
/// cell in pixels. HiDPI screens (tall cells) get two or three device
/// pixels per CSS pixel, so text stays sharp.
pub fn viewport_for(cells: Rect, cell_px: (f64, f64)) -> Viewport {
    let (cw, ch) = cell_px;
    let scale = (ch / 17.0).round().clamp(1.0, 3.0);
    let px = |cells: u16, size: f64| ((cells as f64 * size) / scale).round().max(1.0) as u32;
    Viewport {
        width: px(cells.width, cw),
        height: px(cells.height, ch),
        scale,
    }
}

/// The middle of a cell, in the page's CSS pixels.
pub fn page_point(body: Rect, x: u16, y: u16, cell_px: (f64, f64), v: Viewport) -> (f64, f64) {
    let cx = (x.saturating_sub(body.x) as f64 + 0.5) * cell_px.0 / v.scale;
    let cy = (y.saturating_sub(body.y) as f64 + 0.5) * cell_px.1 / v.scale;
    (cx.min(v.width as f64 - 1.0), cy.min(v.height as f64 - 1.0))
}

/// A pixel on the terminal, in the page's CSS pixels.
pub fn page_point_px(body: Rect, px: (f64, f64), cell_px: (f64, f64), v: Viewport) -> (f64, f64) {
    let x = (px.0 - body.x as f64 * cell_px.0) / v.scale;
    let y = (px.1 - body.y as f64 * cell_px.1) / v.scale;
    (
        x.clamp(0.0, v.width as f64 - 1.0),
        y.clamp(0.0, v.height as f64 - 1.0),
    )
}

/// What the address box makes of what's typed: a URL as is, a bare host
/// over https (http for this machine), anything else a search.
pub fn address(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    let lower = s.to_ascii_lowercase();
    let has_scheme = [
        "http://",
        "https://",
        "file://",
        "about:",
        "data:",
        "chrome://",
    ]
    .iter()
    .any(|p| lower.starts_with(p));
    if has_scheme {
        return s.into();
    }
    let host = lower.split(['/', '?', '#']).next().unwrap_or("");
    let local = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"]
        .iter()
        .any(|h| host == *h || host.starts_with(&format!("{h}:")));
    if local {
        return format!("http://{s}");
    }
    let looks_like_host = !s.contains(char::is_whitespace)
        && (host.contains('.') || host.contains(':'))
        && !host.starts_with('.')
        && !host.ends_with('.');
    if looks_like_host {
        return format!("https://{s}");
    }
    let q: String = s
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://duckduckgo.com/?q={q}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(address("https://x.com/a"), "https://x.com/a");
        assert_eq!(
            address("news.ycombinator.com"),
            "https://news.ycombinator.com"
        );
        assert_eq!(address("localhost:3000/app"), "http://localhost:3000/app");
        assert_eq!(address("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(
            address("rust borrow checker"),
            "https://duckduckgo.com/?q=rust+borrow+checker"
        );
        assert_eq!(address("c++"), "https://duckduckgo.com/?q=c%2B%2B");
        assert_eq!(address(""), "about:blank");
    }

    #[test]
    fn viewport_follows_cells() {
        // A 1x screen: 9x18 cells.
        let v = viewport_for(Rect::new(0, 0, 100, 40), (9.0, 18.0));
        assert_eq!(
            v,
            Viewport {
                width: 900,
                height: 720,
                scale: 1.0
            }
        );
        // Retina: 18x36 cells, two device pixels per CSS pixel.
        let v = viewport_for(Rect::new(0, 0, 100, 40), (18.0, 36.0));
        assert_eq!(
            v,
            Viewport {
                width: 900,
                height: 720,
                scale: 2.0
            }
        );
        let (x, y) = page_point(Rect::new(30, 1, 100, 40), 31, 2, (18.0, 36.0), v);
        assert_eq!((x, y), (13.5, 27.0));
        // From pixels: exact, whatever the cell.
        let (x, y) = page_point_px(
            Rect::new(30, 1, 100, 40),
            (30.0 * 18.0 + 100.0, 36.0 + 51.0),
            (18.0, 36.0),
            v,
        );
        assert_eq!((x, y), (50.0, 25.5));
    }

    #[test]
    fn keys_for_the_page() {
        let a = KeyInput::from_key(Key::Char('a')).unwrap();
        assert_eq!(
            (a.code.as_str(), a.vk, a.text.as_deref()),
            ("KeyA", 65, Some("a"))
        );
        let ctrl_l = KeyInput::from_key(Key::Ctrl('l')).unwrap();
        assert!(ctrl_l.ctrl && ctrl_l.text.is_none());
        assert!(KeyInput::from_key(Key::BackTab).unwrap().shift);
        assert_eq!(KeyInput::from_key(Key::Unknown), None);
    }
}
