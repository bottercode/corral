//! Drawing the page with the kitty graphics protocol, which Ghostty, kitty
//! and WezTerm speak: the PNG is sent once (base64, in chunks), then placed
//! over a block of cells, scaled to fit. A new frame goes to the other of
//! two image ids and replaces the old one in a single step, so the page
//! never flickers blank.

use ratatui::layout::Rect;

/// Our two image ids (any number works; these are unlikely to clash with
/// anything else drawing on the terminal).
const IDS: [u32; 2] = [0x00c0_7a11, 0x00c0_7a12];
/// The kitty protocol takes base64 in chunks of at most 4096 bytes.
const CHUNK: usize = 4096;

/// Whether this terminal can show the page. `CORRAL_GRAPHICS=0/1` decides
/// when the guess is wrong.
pub fn supported() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    match var("CORRAL_GRAPHICS").as_str() {
        "1" | "on" | "yes" => return true,
        "0" | "off" | "no" => return false,
        _ => {}
    }
    // tmux doesn't pass the images through.
    if !var("TMUX").is_empty() {
        return false;
    }
    let term = var("TERM").to_lowercase();
    let program = var("TERM_PROGRAM").to_lowercase();
    term.contains("kitty")
        || term.contains("ghostty")
        || matches!(program.as_str(), "ghostty" | "wezterm" | "kitty")
        || !var("KITTY_WINDOW_ID").is_empty()
}

/// Send a PNG (already base64) as image `id`, without showing it.
pub fn transmit(id: u32, png_base64: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(png_base64.len() + png_base64.len() / CHUNK * 16 + 64);
    let chunks: Vec<&[u8]> = png_base64.as_bytes().chunks(CHUNK).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.extend_from_slice(format!("\x1b_Ga=t,f=100,t=d,i={id},q=2,m={more};").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Show image `id` over `area` (cells), scaled to fill it. The cursor is
/// put back where it was.
pub fn place(id: u32, area: Rect) -> Vec<u8> {
    format!(
        "\x1b7\x1b[{};{}H\x1b_Ga=p,i={id},p=1,c={},r={},C=1,q=2\x1b\\\x1b8",
        area.y + 1,
        area.x + 1,
        area.width,
        area.height
    )
    .into_bytes()
}

/// Take image `id` off the screen; `free` also drops its data.
pub fn delete(id: u32, free: bool) -> Vec<u8> {
    let d = if free { 'I' } else { 'i' };
    format!("\x1b_Ga=d,d={d},i={id},q=2\x1b\\").into_bytes()
}

/// What's on the terminal, so only changes are sent.
#[derive(Debug, Default)]
pub struct Gfx {
    /// The image whose data the terminal has, and which frame it is.
    data: Option<(u32, u64)>,
    /// Where it's shown.
    shown: Option<Rect>,
}

impl Gfx {
    /// Bytes that bring the terminal to showing `frame` (`(seq, png)`) over
    /// `area`, or nothing when `area` is `None`.
    pub fn update(&mut self, area: Option<Rect>, frame: Option<(u64, &str)>) -> Vec<u8> {
        let mut out = Vec::new();
        let (Some(area), Some((seq, png))) = (area, frame) else {
            if self.shown.take().is_some()
                && let Some((id, _)) = self.data
            {
                out.extend(delete(id, false));
            }
            return out;
        };
        match self.data {
            Some((id, have)) if have == seq => {
                if self.shown != Some(area) {
                    out.extend(place(id, area));
                }
            }
            old => {
                // The new frame goes to the other id, then replaces the old.
                let id = match old {
                    Some((old_id, _)) if old_id == IDS[0] => IDS[1],
                    _ => IDS[0],
                };
                out.extend(transmit(id, png));
                out.extend(place(id, area));
                if let Some((old_id, _)) = old {
                    out.extend(delete(old_id, true));
                }
                self.data = Some((id, seq));
            }
        }
        self.shown = Some(area);
        out
    }

    /// The screen was cleared (a resize): placements are gone, data isn't.
    pub fn forget_placement(&mut self) {
        self.shown = None;
    }

    /// Everything of ours, off the terminal (on exit).
    pub fn clear(&mut self) -> Vec<u8> {
        self.shown = None;
        self.data = None;
        IDS.iter().flat_map(|id| delete(*id, true)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmits_in_chunks() {
        let png = "A".repeat(CHUNK * 2 + 10);
        let out = String::from_utf8(transmit(5, &png)).unwrap();
        let parts: Vec<&str> = out.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("\x1b_Ga=t,f=100,t=d,i=5,q=2,m=1;AAAA"));
        assert!(parts[1].starts_with("\x1b_Gm=1;"));
        assert!(parts[2].starts_with("\x1b_Gm=0;"));
        assert_eq!(parts[2].len(), "\x1b_Gm=0;".len() + 10);
    }

    #[test]
    fn places_at_the_area() {
        let out = String::from_utf8(place(5, Rect::new(30, 1, 80, 20))).unwrap();
        assert_eq!(
            out,
            "\x1b7\x1b[2;31H\x1b_Ga=p,i=5,p=1,c=80,r=20,C=1,q=2\x1b\\\x1b8"
        );
    }

    #[test]
    fn only_changes_are_sent() {
        let mut g = Gfx::default();
        let a = Rect::new(0, 0, 10, 5);
        let first = String::from_utf8(g.update(Some(a), Some((1, "PNG1")))).unwrap();
        assert!(first.contains("a=t") && first.contains("PNG1") && first.contains("a=p"));
        // Same frame, same place: nothing.
        assert!(g.update(Some(a), Some((1, "PNG1"))).is_empty());
        // Moved: placed again, not sent again.
        let moved =
            String::from_utf8(g.update(Some(Rect::new(5, 0, 10, 5)), Some((1, "PNG1")))).unwrap();
        assert!(moved.contains("a=p") && !moved.contains("a=t"));
        // New frame: the other id, and the old one freed.
        let next = String::from_utf8(g.update(Some(a), Some((2, "PNG2")))).unwrap();
        assert!(next.contains(&format!("i={}", IDS[1])) && next.contains("PNG2"));
        assert!(next.contains(&format!("a=d,d=I,i={}", IDS[0])));
        // Hidden (a dialog on top), then back without resending.
        let hide = String::from_utf8(g.update(None, Some((2, "PNG2")))).unwrap();
        assert_eq!(hide, format!("\x1b_Ga=d,d=i,i={},q=2\x1b\\", IDS[1]));
        assert!(g.update(None, Some((2, "PNG2"))).is_empty());
        let back = String::from_utf8(g.update(Some(a), Some((2, "PNG2")))).unwrap();
        assert!(back.contains("a=p") && !back.contains("a=t"));
    }
}
