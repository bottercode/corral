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
            r.all.iter_mut().for_each(|s| *s = s.to_lowercase());
            r.any.iter_mut().for_each(|s| *s = s.to_lowercase());
            rules.push((verdict, r));
        }
        Ok(Rules { rules })
    }

    /// Classify a screen for a pane running `agent` (if known).
    pub fn classify(&self, agent: Option<&str>, view: View) -> Verdict {
        let screen = bottom_rows(view.text, SCAN_ROWS).to_lowercase();
        let line = view.cursor_line.to_lowercase();
        let applies = |r: &Rule| r.agent == "*" || Some(r.agent.as_str()) == agent;
        let matches = |r: &Rule| {
            let text = match r.scope {
                Scope::Screen => &screen,
                Scope::CursorLine => &line,
            };
            r.all.iter().all(|s| text.contains(s.as_str()))
                && (r.any.is_empty() || r.any.iter().any(|s| text.contains(s.as_str())))
        };
        let mut verdict = Verdict::None;
        for (v, r) in &self.rules {
            if applies(r) && matches(r) {
                match v {
                    Verdict::Blocked => return Verdict::Blocked,
                    Verdict::Working => verdict = Verdict::Working,
                    Verdict::None => {}
                }
            }
        }
        verdict
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
    fn bottom_rows_helper() {
        assert_eq!(bottom_rows("a\nb\nc\n\n", 2), "b\nc");
        assert_eq!(bottom_rows("a", 3), "a");
    }
}
