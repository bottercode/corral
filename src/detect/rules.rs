//! Data-driven screen rules (see `rules.toml`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::Verdict;

const BUILTIN: &str = include_str!("rules.toml");

/// Only the bottom of the screen is searched; prompts and spinners live
/// there, and old answered prompts higher up shouldn't count.
pub const SCAN_ROWS: usize = 30;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub agent: String,
    pub state: String,
    #[serde(default)]
    pub all: Vec<String>,
    #[serde(default)]
    pub any: Vec<String>,
    #[serde(default)]
    pub scope: Scope,
    /// Search only this many bottom rows instead of [`SCAN_ROWS`].
    #[serde(default)]
    pub rows: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    #[default]
    Screen,
    CursorLine,
}

/// What the rules look at.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    pub text: &'a str,
    /// The line the cursor is on.
    pub cursor_line: &'a str,
}

impl<'a> View<'a> {
    /// A view of `text` with the cursor on its last non-empty line.
    pub fn from_text(text: &'a str) -> Self {
        let cursor_line = text.trim_end().rsplit('\n').next().unwrap_or("");
        View { text, cursor_line }
    }

    /// A view of a vt100 screen; `text` must be `screen.contents()`.
    pub fn of_screen(screen: &vt100::Screen, text: &'a str, cursor_line: &'a mut String) -> Self {
        let (row, _) = screen.cursor_position();
        let (_, cols) = screen.size();
        *cursor_line = screen.rows(0, cols).nth(row as usize).unwrap_or_default();
        View { text, cursor_line }
    }
}

#[derive(Debug, Deserialize)]
struct File {
    rule: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct Rules {
    rules: Vec<(Verdict, Rule)>,
}

impl Rules {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("built-in rules.toml is valid")
    }

    pub fn parse(src: &str) -> Result<Self> {
        let file: File = toml::from_str(src).context("parsing rules")?;
        let mut rules = Vec::new();
        for mut r in file.rule {
            let verdict = match r.state.as_str() {
                "blocked" => Verdict::Blocked,
                "working" => Verdict::Working,
                other => bail!("rule state must be blocked or working, not {other:?}"),
            };
            if r.all.is_empty() && r.any.is_empty() {
                bail!("rule for {:?} has neither `all` nor `any`", r.agent);
            }
            if r.rows == Some(0) {
                bail!("rule for {:?} has `rows = 0`", r.agent);
            }
            r.all.iter_mut().for_each(|s| *s = s.to_lowercase());
            r.any.iter_mut().for_each(|s| *s = s.to_lowercase());
            rules.push((verdict, r));
        }
        Ok(Rules { rules })
    }

    /// Classify a screen for a pane running `agent` (if known).
    ///
    /// Where a rule matched counts: a working footer below a blocked match
    /// means the "prompt" is old text in the transcript (or the agent
    /// printing a prompt-like snippet) and the agent is working.
    pub fn classify(&self, agent: Option<&str>, view: View) -> Verdict {
        let screen = view.text.trim_end().to_lowercase();
        let line = view.cursor_line.to_lowercase();
        let applies = |r: &Rule| r.agent == "*" || Some(r.agent.as_str()) == agent;
        // How far down the screen the rule matched: the end of its lowest
        // needed string, as an offset into `screen`.
        let position = |r: &Rule| -> Option<usize> {
            let (text, base) = match r.scope {
                Scope::Screen => {
                    let text = bottom_rows(&screen, r.rows.unwrap_or(SCAN_ROWS));
                    (text, screen.len() - text.len())
                }
                // The cursor is where the program waits: as low as it gets.
                Scope::CursorLine => (line.as_str(), usize::MAX),
            };
            let last = |s: &String| text.rfind(s.as_str()).map(|i| i + s.len());
            let mut pos = 0;
            for s in &r.all {
                pos = pos.max(last(s)?);
            }
            if !r.any.is_empty() {
                pos = pos.max(r.any.iter().filter_map(last).max()?);
            }
            Some(base.saturating_add(pos))
        };
        let (mut blocked, mut working) = (None, None);
        for (v, r) in &self.rules {
            if !applies(r) {
                continue;
            }
            let Some(pos) = position(r) else { continue };
            let best = match v {
                Verdict::Blocked => &mut blocked,
                Verdict::Working => &mut working,
                Verdict::None => continue,
            };
            *best = Some(best.map_or(pos, |b: usize| b.max(pos)));
        }
        match (blocked, working) {
            (Some(b), Some(w)) if w > b => Verdict::Working,
            (Some(_), _) => Verdict::Blocked,
            (None, Some(_)) => Verdict::Working,
            (None, None) => Verdict::None,
        }
    }
}

fn bottom_rows(text: &str, n: usize) -> &str {
    let trimmed = text.trim_end();
    match trimmed.match_indices('\n').rev().nth(n.saturating_sub(1)) {
        Some((i, _)) => &trimmed[i + 1..],
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_parse() {
        let r = Rules::builtin();
        assert!(r.rules.len() > 5);
    }

    #[test]
    fn bad_rules_are_rejected() {
        assert!(Rules::parse("[[rule]]\nagent='x'\nstate='sleepy'\nall=['a']").is_err());
        assert!(Rules::parse("[[rule]]\nagent='x'\nstate='blocked'").is_err());
        assert!(
            Rules::parse("[[rule]]\nagent='x'\nstate='blocked'\nall=['a']\nscope='nope'").is_err()
        );
        assert!(Rules::parse("[[rule]]\nagent='x'\nstate='blocked'\nall=['a']\nrows=0").is_err());
    }

    #[test]
    fn generic_yes_no() {
        let r = Rules::builtin();
        let c = |a, t| r.classify(a, View::from_text(t));
        assert_eq!(c(None, "Overwrite file? [Y/n] "), Verdict::Blocked);
        assert_eq!(c(Some("claude"), "Continue (y/N)?"), Verdict::Blocked);
        assert_eq!(c(None, "$ ls\nfoo bar\n$ "), Verdict::None);
        // Answered: the prompt is still on screen but the cursor moved on.
        assert_eq!(c(None, "Overwrite file? [Y/n] y\nwriting…"), Verdict::None);
    }

    #[test]
    fn agent_rules_only_apply_to_their_agent() {
        let r = Rules::builtin();
        let text = View::from_text("Yes, I trust this folder");
        assert_eq!(r.classify(Some("claude"), text), Verdict::Blocked);
        assert_eq!(r.classify(Some("codex"), text), Verdict::None);
    }

    #[test]
    fn only_the_bottom_rows_count() {
        let r = Rules::builtin();
        let mut text = String::from("do you want to proceed?\n❯ 1. yes\n");
        text.push_str(&"line\n".repeat(SCAN_ROWS + 5));
        assert_eq!(
            r.classify(Some("claude"), View::from_text(&text)),
            Verdict::None
        );
    }

    #[test]
    fn prompt_text_above_a_spinner_is_working() {
        // Claude editing a script that prints a permission prompt: the
        // snippet is in the transcript, the spinner footer is below it.
        let r = Rules::builtin();
        let text = "⏺ Update(bin/claude)\n  printf 'Do you want to proceed?'\n  printf '❯ 1. Yes'\n\n\
                    ✻ Concocting… (9s)\n─────\n❯ \n─────\n  ⏵⏵ auto mode on · esc to interrupt";
        assert_eq!(
            r.classify(Some("claude"), View::from_text(text)),
            Verdict::Working
        );
        // A live prompt replaces the input box; an old spinner line in the
        // transcript above it doesn't hide it.
        let text =
            "old: esc to interrupt\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n\nEsc to cancel";
        assert_eq!(
            r.classify(Some("claude"), View::from_text(text)),
            Verdict::Blocked
        );
    }

    #[test]
    fn claude_prompts_must_be_near_the_bottom() {
        let r = Rules::builtin();
        let mut text = String::from("Do you want to proceed?\n❯ 1. Yes\n");
        text.push_str(&"transcript\n".repeat(12));
        text.push_str("❯ \n? for shortcuts");
        assert_eq!(
            r.classify(Some("claude"), View::from_text(&text)),
            Verdict::None
        );
    }

    #[test]
    fn bottom_rows_helper() {
        assert_eq!(bottom_rows("a\nb\nc\n\n", 2), "b\nc");
        assert_eq!(bottom_rows("a", 3), "a");
    }
}
