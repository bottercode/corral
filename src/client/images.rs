//! Previews of images pasted into an agent.
//!
//! Agents like Claude Code read a pasted image straight from the system
//! clipboard when Ctrl-V reaches them, so it never passes through the
//! terminal and nothing shows it. corral sees the Ctrl-V go by, reads the
//! same clipboard and shows the image on a card over the pane until the
//! prompt is sent. Dropping an image file on the terminal pastes its path,
//! which works the same way.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, FontSize, Resize};

/// The most room a preview takes inside its card, in cells.
pub const MAX_CELLS: Size = Size::new(32, 8);

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Clipboard,
    File(PathBuf),
}

/// One pasted image, ready to draw.
pub struct Preview {
    pub protocol: Protocol,
    /// The image's own size in pixels.
    pub width: u32,
    pub height: u32,
}

impl Preview {
    /// The cells the image covers.
    pub fn cells(&self) -> Size {
        self.protocol.size()
    }
}

/// How this terminal draws images, from what it says about itself. Asking
/// it directly would mean reading its answer from stdin, which is the stdin
/// thread's, and a terminal that never answers would leave a reader behind.
pub fn picker() -> Picker {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let protocol = graphics(
        &env("TERM"),
        &env("TERM_PROGRAM"),
        !env("KITTY_WINDOW_ID").is_empty(),
        !env("TMUX").is_empty(),
    );
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font_size().unwrap_or(FontSize::new(10, 20)));
    picker.set_protocol_type(protocol);
    picker
}

/// The image protocol for a terminal: kitty's where it's known to work with
/// placeholders, iTerm2's where that's the one, and coloured half blocks
/// everywhere else (tmux included, which passes neither through reliably).
fn graphics(term: &str, program: &str, kitty_window: bool, tmux: bool) -> ProtocolType {
    if tmux {
        return ProtocolType::Halfblocks;
    }
    match program {
        "ghostty" => ProtocolType::Kitty,
        "iTerm.app" | "WezTerm" => ProtocolType::Iterm2,
        _ if term == "xterm-kitty" || term == "xterm-ghostty" || kitty_window => {
            ProtocolType::Kitty
        }
        _ => ProtocolType::Halfblocks,
    }
}

/// A cell's size in pixels, if the terminal reports its own.
fn font_size() -> Option<FontSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills in a winsize.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if !ok || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some(FontSize::new(
        ws.ws_xpixel / ws.ws_col,
        ws.ws_ypixel / ws.ws_row,
    ))
}

/// Ctrl-V, as plain terminals and the kitty keyboard protocol send it.
pub fn is_paste_key(bytes: &[u8]) -> bool {
    bytes == b"\x16" || bytes == b"\x1b[118;5u"
}

/// The image files a paste names, when that's all it is: what dropping
/// files on a terminal types. Paths come escaped (`a\ b.png`), quoted, or as
/// `file://` URLs.
pub fn image_paths(paste: &str) -> Vec<PathBuf> {
    let paste = paste.trim();
    if paste.is_empty() || paste.len() > 8192 || paste.contains('\n') {
        return Vec::new();
    }
    let paths: Vec<PathBuf> = split_words(paste)
        .into_iter()
        .map(|w| match w.strip_prefix("file://") {
            Some(url) => PathBuf::from(percent_decode(url)),
            None => PathBuf::from(w),
        })
        .collect();
    if paths.iter().all(|p| is_image_file(p)) {
        paths
    } else {
        Vec::new()
    }
}

fn is_image_file(p: &Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "tif" | "tiff" | "bmp")
    ) && p.is_absolute()
        && p.is_file()
}

/// `%20` and friends in a `file://` URL's path.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Shell-like words: spaces split them, unless escaped or quoted.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') => word.extend(chars.next()),
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Read the image and make its preview; `None` when the clipboard holds no
/// image. Blocking: run it off the UI thread.
pub fn load(source: &Source, picker: &Picker) -> Result<Option<Preview>> {
    let bytes = match source {
        Source::Clipboard => match clipboard_image()? {
            Some(b) => b,
            None => return Ok(None),
        },
        Source::File(p) => std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
    };
    let image = decode(&bytes)?;
    let (width, height) = (image.width(), image.height());
    // Shrink big images before the picker fits them to cells.
    let font = picker.font_size();
    let image = image.thumbnail(
        u32::from(MAX_CELLS.width) * u32::from(font.width) * 2,
        u32::from(MAX_CELLS.height) * u32::from(font.height) * 2,
    );
    let protocol = picker
        .new_protocol(image, MAX_CELLS, Resize::Fit(Some(FilterType::Triangle)))
        .context("encoding the preview")?;
    Ok(Some(Preview {
        protocol,
        width,
        height,
    }))
}

fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("decoding the image")
}

/// The clipboard's image, as file bytes, if it holds one.
fn clipboard_image() -> Result<Option<Vec<u8>>> {
    if cfg!(target_os = "macos") {
        for class in ["PNGf", "TIFF"] {
            let out = run(
                "osascript",
                &["-e", &format!("the clipboard as «class {class}»")],
            )?;
            if let Some(bytes) = parse_applescript_data(&out) {
                return Ok(Some(bytes));
            }
        }
        return Ok(None);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let types = run("wl-paste", &["--list-types"])?;
        return match image_type(&types) {
            Some(t) => Ok(Some(run_bytes("wl-paste", &["--no-newline", "--type", t])?)),
            None => Ok(None),
        };
    }
    if std::env::var_os("DISPLAY").is_some() {
        let types = run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])?;
        return match image_type(&types) {
            Some(t) => Ok(Some(run_bytes(
                "xclip",
                &["-selection", "clipboard", "-t", t, "-o"],
            )?)),
            None => Ok(None),
        };
    }
    Ok(None)
}

/// The first image type in a list of clipboard types, PNG first.
fn image_type(types: &str) -> Option<&str> {
    let mut types = types.lines().map(str::trim);
    let all: Vec<&str> = types.by_ref().filter(|t| t.starts_with("image/")).collect();
    all.iter()
        .find(|t| **t == "image/png")
        .or(all.first())
        .copied()
}

/// `«data PNGf89504E47…»`, the way AppleScript prints binary data.
fn parse_applescript_data(out: &str) -> Option<Vec<u8>> {
    let hex = out
        .trim()
        .strip_prefix("«data ")?
        .strip_suffix('»')?
        .get(4..)?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    // Not holding an image (or the tool missing) is just no image.
    match run_bytes(cmd, args) {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(_) => Ok(String::new()),
    }
}

fn run_bytes(cmd: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} failed");
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_keys() {
        assert!(is_paste_key(b"\x16"));
        assert!(is_paste_key(b"\x1b[118;5u"));
        assert!(!is_paste_key(b"v"));
        assert!(!is_paste_key(b"\x16\x16"));
    }

    #[test]
    fn protocols() {
        use ProtocolType::*;
        assert_eq!(graphics("xterm-ghostty", "ghostty", false, false), Kitty);
        assert_eq!(graphics("xterm-kitty", "", true, false), Kitty);
        assert_eq!(
            graphics("xterm-256color", "iTerm.app", false, false),
            Iterm2
        );
        assert_eq!(graphics("xterm-256color", "WezTerm", false, false), Iterm2);
        assert_eq!(
            graphics("xterm-256color", "Apple_Terminal", false, false),
            Halfblocks
        );
        // tmux gets half blocks whatever is outside it.
        assert_eq!(
            graphics("tmux-256color", "ghostty", false, true),
            Halfblocks
        );
    }

    #[test]
    fn words() {
        assert_eq!(
            split_words(r"/a/b\ c.png /d.png"),
            vec!["/a/b c.png", "/d.png"]
        );
        assert_eq!(
            split_words("'/a b.png' \"/c d.png\""),
            vec!["/a b.png", "/c d.png"]
        );
    }

    #[test]
    fn dropped_files() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot one.png");
        DynamicImage::new_rgb8(4, 2).save(&png).unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "x").unwrap();
        let escaped = png.to_str().unwrap().replace(' ', "\\ ");
        assert_eq!(image_paths(&escaped), vec![png.clone()]);
        let url = format!("file://{}", png.display()).replace(' ', "%20");
        assert_eq!(image_paths(&url), vec![png.clone()]);
        // Anything that isn't only image files is just text.
        assert!(image_paths(&format!("{escaped} {}", txt.display())).is_empty());
        assert!(image_paths("look at this").is_empty());
        assert!(image_paths("/no/such/file.png").is_empty());
    }

    #[test]
    fn applescript_data() {
        assert_eq!(
            parse_applescript_data("«data PNGf89504E47»\n"),
            Some(vec![0x89, 0x50, 0x4E, 0x47])
        );
        assert_eq!(parse_applescript_data(""), None);
        assert_eq!(parse_applescript_data("some text"), None);
    }

    #[test]
    fn clipboard_types() {
        assert_eq!(
            image_type("text/plain\nimage/jpeg\nimage/png\n"),
            Some("image/png")
        );
        assert_eq!(image_type("TARGETS\nimage/jpeg"), Some("image/jpeg"));
        assert_eq!(image_type("UTF8_STRING\ntext/plain"), None);
    }

    #[test]
    fn previews_fit_their_card() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("wide.png");
        DynamicImage::new_rgb8(1600, 400).save(&png).unwrap();
        let p = load(&Source::File(png), &Picker::halfblocks())
            .unwrap()
            .unwrap();
        assert_eq!((p.width, p.height), (1600, 400));
        let cells = p.cells();
        assert!(cells.width <= MAX_CELLS.width && cells.height <= MAX_CELLS.height);
        assert!(cells.width > 0 && cells.height > 0);
    }
}
