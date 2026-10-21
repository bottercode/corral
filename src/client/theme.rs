//! corral's colours. Everything the TUI draws asks the theme for a role
//! (accent, blocked, muted…) rather than naming a colour, so a theme is just
//! one table. `theme = "ranch" | "ink" | "terminal"` in the config.
//!
//! - **ranch** (default): warm field colours — hay for the accent, rust for
//!   agents that are blocked, sage for finished work, creek for work in
//!   progress, heather for git. Draws on the terminal's own background.
//! - **ink**: greys, with one amber signal kept for what needs you.
//! - **terminal**: the terminal's 16 ANSI colours, so it matches whatever
//!   palette the terminal already has.

use std::sync::OnceLock;

use anyhow::{Result, bail};
use ratatui::style::{Color, Modifier, Style};

use crate::protocol::AgentState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    /// corral's own colour: the wordmark, keys, focus marks, dialog frames.
    pub accent: Color,
    /// Text drawn on an `accent` background.
    pub on_accent: Color,
    pub text: Color,
    /// Secondary text: labels, hints, quiet agents.
    pub muted: Color,
    /// Rules and separators.
    pub faint: Color,
    /// Background of the focused sidebar row; `None` leaves it unpainted.
    pub selection: Option<Color>,
    pub blocked: Color,
    pub done: Color,
    pub working: Color,
    /// Branches and worktrees.
    pub git: Color,
    pub added: Color,
    pub removed: Color,
    pub error: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const RANCH: Theme = Theme {
    name: "ranch",
    accent: rgb(0xE3B25B),
    on_accent: rgb(0x1E1B16),
    text: Color::Reset,
    muted: rgb(0x8E877C),
    faint: rgb(0x4B463F),
    selection: Some(rgb(0x2D2924)),
    blocked: rgb(0xDB6B47),
    done: rgb(0x93B57F),
    working: rgb(0x6FAFB8),
    git: rgb(0xB79BC9),
    added: rgb(0x93B57F),
    removed: rgb(0xDB6B47),
    error: rgb(0xE5674F),
};

pub const INK: Theme = Theme {
    name: "ink",
    accent: rgb(0xD9D6D0),
    on_accent: rgb(0x161616),
    text: Color::Reset,
    muted: rgb(0x85837F),
    faint: rgb(0x403F3D),
    selection: Some(rgb(0x272625)),
    blocked: rgb(0xF0A63A),
    done: rgb(0xC9A46A),
    working: rgb(0xA8A6A2),
    git: rgb(0x9C9A96),
    added: rgb(0xB9B6B0),
    removed: rgb(0xF0A63A),
    error: rgb(0xF0A63A),
};

pub const TERMINAL: Theme = Theme {
    name: "terminal",
    accent: Color::Yellow,
    on_accent: Color::Black,
    text: Color::Reset,
    muted: Color::DarkGray,
    faint: Color::DarkGray,
    selection: None,
    blocked: Color::Red,
    done: Color::Green,
    working: Color::Cyan,
    git: Color::Magenta,
    added: Color::Green,
    removed: Color::Red,
    error: Color::Red,
};

pub const ALL: [Theme; 3] = [RANCH, INK, TERMINAL];

static CURRENT: OnceLock<Theme> = OnceLock::new();

pub fn by_name(name: &str) -> Result<Theme> {
    match ALL
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name.trim()))
    {
        Some(t) => Ok(*t),
        None => bail!(
            "unknown theme {name:?}; pick one of {}",
            ALL.map(|t| t.name).join(", ")
        ),
    }
}

/// Choose the theme for this process (first call wins).
pub fn set(theme: Theme) {
    let _ = CURRENT.set(theme);
}

/// The theme in use; ranch until `set` is called.
pub fn get() -> &'static Theme {
    CURRENT.get().unwrap_or(&RANCH)
}

impl Theme {
    pub fn fg(&self, c: Color) -> Style {
        Style::new().fg(c)
    }

    pub fn accent(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn faint(&self) -> Style {
        Style::new().fg(self.faint)
    }

    pub fn bold(&self) -> Style {
        Style::new().fg(self.text).add_modifier(Modifier::BOLD)
    }

    /// A label set in reverse on the accent (`^b` while it waits for a key).
    pub fn on_accent_style(&self) -> Style {
        Style::new().fg(self.on_accent).bg(self.accent)
    }

    /// A key in a hint (`n`, `Enter`, `^b`).
    pub fn key(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn state(&self, s: AgentState) -> Color {
        match s {
            AgentState::Blocked => self.blocked,
            AgentState::Done => self.done,
            AgentState::Working => self.working,
            AgentState::Idle | AgentState::Exited => self.muted,
        }
    }

    /// Background for the focused row, if the theme paints one.
    pub fn selected(&self) -> Style {
        match self.selection {
            Some(bg) => Style::new().bg(bg),
            None => Style::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_by_name() {
        assert_eq!(by_name("ranch").unwrap(), RANCH);
        assert_eq!(by_name(" Ink ").unwrap(), INK);
        assert_eq!(by_name("terminal").unwrap(), TERMINAL);
        let e = by_name("herd").unwrap_err().to_string();
        assert!(e.contains("ranch, ink, terminal"), "{e}");
    }
}
