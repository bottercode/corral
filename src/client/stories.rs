//! Top stories from Hacker News and lobste.rs, on a page drawn over the
//! panes (`^b t`). Both sites have public JSON APIs that need no key.
//! Nothing is fetched until the page opens, and a feed is reused for a few
//! minutes. Links open in the system browser.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::sidebar::{short_duration, truncate};
use super::theme;

/// A feed older than this is fetched again when the page is open.
pub const FRESH_FOR: Duration = Duration::from_secs(5 * 60);
/// Stories per feed.
const COUNT: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Rows per story: title, details, a gap.
const STORY_ROWS: u16 = 3;
/// Widest the column of stories gets; wider panes center it.
const MAX_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    HackerNews,
    Lobsters,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::HackerNews, Source::Lobsters];

    pub fn label(self) -> &'static str {
        match self {
            Source::HackerNews => "Hacker News",
            Source::Lobsters => "lobste.rs",
        }
    }

    pub fn other(self) -> Source {
        match self {
            Source::HackerNews => Source::Lobsters,
            Source::Lobsters => Source::HackerNews,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Story {
    pub title: String,
    /// The link; `None` for text posts (Ask HN and the like).
    pub url: Option<String>,
    /// The comments page.
    pub discussion: String,
    pub points: u64,
    pub comments: u64,
    pub by: String,
    /// Unix seconds.
    pub posted: i64,
    pub tags: Vec<String>,
}

impl Story {
    /// The link's host without `www.`, shown next to the title.
    pub fn site(&self) -> Option<&str> {
        let url = self.url.as_deref()?;
        let rest = url.split_once("://")?.1;
        let host = rest.split(['/', '?', '#']).next()?;
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        Some(host.strip_prefix("www.").unwrap_or(host)).filter(|h| !h.is_empty())
    }

    /// Where Enter goes: the link, or the comments for a text post.
    pub fn link(&self) -> &str {
        self.url.as_deref().unwrap_or(&self.discussion)
    }
}

// Fetching. Blocking: the client runs it on a worker thread.

pub fn fetch(source: Source) -> Result<Vec<Story>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("corral/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    match source {
        Source::HackerNews => fetch_hacker_news(&agent),
        Source::Lobsters => parse_lobsters(&get(&agent, "https://lobste.rs/hottest.json")?),
    }
}

fn get(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .with_context(|| format!("fetching {url}"))
}

/// The front page's ids, then each story, fetched side by side.
fn fetch_hacker_news(agent: &ureq::Agent) -> Result<Vec<Story>> {
    const API: &str = "https://hacker-news.firebaseio.com/v0";
    let ids: Vec<u64> = serde_json::from_str(&get(agent, &format!("{API}/topstories.json"))?)
        .context("reading the Hacker News front page")?;
    let items: Vec<Option<Story>> = std::thread::scope(|s| {
        let jobs: Vec<_> = ids
            .iter()
            .take(COUNT)
            .map(|id| {
                s.spawn(move || {
                    let body = get(agent, &format!("{API}/item/{id}.json")).ok()?;
                    parse_hacker_news_item(&serde_json::from_str(&body).ok()?)
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    let stories: Vec<Story> = items.into_iter().flatten().collect();
    if stories.is_empty() {
        bail!("Hacker News sent no stories");
    }
    Ok(stories)
}

/// One item from the Hacker News API; `None` for dead or deleted ones.
pub fn parse_hacker_news_item(v: &Value) -> Option<Story> {
    if v["dead"].as_bool() == Some(true) || v["deleted"].as_bool() == Some(true) {
        return None;
    }
    let id = v["id"].as_u64()?;
    Some(Story {
        title: clean(v["title"].as_str()?),
        url: v["url"].as_str().map(clean).filter(|u| openable(u)),
        discussion: format!("https://news.ycombinator.com/item?id={id}"),
        points: v["score"].as_u64().unwrap_or(0),
        comments: v["descendants"].as_u64().unwrap_or(0),
        by: clean(v["by"].as_str().unwrap_or("")),
        posted: v["time"].as_i64().unwrap_or(0),
        tags: Vec::new(),
    })
}

/// lobste.rs's `hottest.json`.
pub fn parse_lobsters(body: &str) -> Result<Vec<Story>> {
    let items: Vec<Value> = serde_json::from_str(body).context("reading lobste.rs")?;
    let stories: Vec<Story> = items
        .iter()
        .filter_map(|v| {
            // `submitter_user` is a name, or an object in older replies.
            let by = v["submitter_user"]
                .as_str()
                .or_else(|| v["submitter_user"]["username"].as_str())
                .unwrap_or("");
            Some(Story {
                title: clean(v["title"].as_str()?),
                url: v["url"].as_str().map(clean).filter(|u| openable(u)),
                discussion: v["comments_url"]
                    .as_str()
                    .or(v["short_id_url"].as_str())
                    .map(clean)
                    .filter(|u| openable(u))?,
                points: v["score"].as_u64().unwrap_or(0),
                comments: v["comment_count"].as_u64().unwrap_or(0),
                by: clean(by),
                posted: v["created_at"].as_str().and_then(unix_time).unwrap_or(0),
                tags: v["tags"]
                    .as_array()
                    .map(|t| t.iter().filter_map(Value::as_str).map(clean).collect())
                    .unwrap_or_default(),
            })
        })
        .take(COUNT)
        .collect();
    if stories.is_empty() {
        bail!("lobste.rs sent no stories");
    }
    Ok(stories)
}

/// Text from the network goes on the terminal: no control characters (an
/// escape sequence in a title would be written straight to it).
fn clean(s: &str) -> String {
    s.split(char::is_whitespace)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// `2026-10-20T17:23:54.452-05:00` → Unix seconds.
fn unix_time(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // The offset comes after the seconds and any fraction.
    let tail = s
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match tail.as_bytes().first() {
        Some(b'Z') | None => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let oh: i64 = tail.get(1..3)?.parse().ok()?;
            let om: i64 = tail.get(4..6)?.parse().ok()?;
            let o = oh * 3600 + om * 60;
            if sign == b'-' { -o } else { o }
        }
        _ => return None,
    };
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Only web links get handed to the browser.
pub fn openable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")) && !url.contains(' ')
}

/// Open a link in the system browser (`open` on macOS, `xdg-open` elsewhere).
pub fn open_in_browser(url: &str) -> Result<()> {
    if !openable(url) {
        bail!("not a web link: {url}");
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running {opener}"))?;
    // Reap it without waiting here.
    std::thread::spawn(move || child.wait());
    Ok(())
}

// The page.

#[derive(Debug, Default)]
pub struct Feed {
    pub stories: Vec<Story>,
    pub fetched: Option<Instant>,
    pub loading: bool,
    pub error: Option<String>,
}

/// What a click on the page does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Source(Source),
    Open(usize),
    Comments(usize),
}

/// The feeds, and where the reader is.
#[derive(Debug)]
pub struct Stories {
    pub source: Source,
    feeds: HashMap<Source, Feed>,
    pub selected: usize,
    /// The first story drawn.
    pub top: usize,
    /// Stories that fit, as of the last draw.
    pub visible: usize,
    pub hits: Vec<(Rect, Click)>,
}

impl Default for Stories {
    fn default() -> Self {
        Stories {
            source: Source::HackerNews,
            feeds: HashMap::new(),
            selected: 0,
            top: 0,
            visible: 1,
            hits: Vec::new(),
        }
    }
}

impl Stories {
    pub fn feed(&self) -> Option<&Feed> {
        self.feeds.get(&self.source)
    }

    fn len(&self) -> usize {
        self.feed().map_or(0, |f| f.stories.len())
    }

    pub fn story(&self, i: usize) -> Option<&Story> {
        self.feed()?.stories.get(i)
    }

    pub fn current(&self) -> Option<&Story> {
        self.story(self.selected)
    }

    /// The shown source, if it should be fetched now (never fetched, stale,
    /// or `force`); it's marked as loading.
    pub fn want(&mut self, force: bool) -> Option<Source> {
        let feed = self.feeds.entry(self.source).or_default();
        let stale = feed.fetched.is_none_or(|t| t.elapsed() >= FRESH_FOR);
        if feed.loading || !(stale || force) {
            return None;
        }
        feed.loading = true;
        Some(self.source)
    }

    pub fn loaded(&mut self, source: Source, result: Result<Vec<Story>, String>) {
        let feed = self.feeds.entry(source).or_default();
        feed.loading = false;
        match result {
            Ok(stories) => {
                feed.stories = stories;
                feed.fetched = Some(Instant::now());
                feed.error = None;
                if source == self.source {
                    self.selected = self.selected.min(self.len().saturating_sub(1));
                }
            }
            Err(e) => feed.error = Some(e),
        }
    }

    pub fn switch(&mut self, source: Source) {
        if source != self.source {
            self.source = source;
            self.selected = 0;
            self.top = 0;
        }
    }

    /// Move the selection, scrolling to keep it on the page.
    pub fn move_by(&mut self, step: isize) {
        let last = self.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(step).min(last);
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.visible {
            self.top = self.selected + 1 - self.visible;
        }
    }

    /// Scroll the page (the wheel); the selection stays on it.
    pub fn scroll_by(&mut self, step: isize) {
        let max_top = self.len().saturating_sub(self.visible);
        self.top = self.top.saturating_add_signed(step).min(max_top);
        self.selected = self
            .selected
            .clamp(self.top, self.top + self.visible.saturating_sub(1));
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }

    pub fn click_at(&self, x: u16, y: u16) -> Option<Click> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains((x, y).into()))
            .map(|&(_, c)| c)
    }

    /// Draw the page into `area`; `now` is Unix seconds, for ages.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, now: i64) {
        let t = theme::get();
        self.hits.clear();
        if area.width < 20 || area.height < 4 {
            return;
        }
        let width = area.width.saturating_sub(4).min(MAX_WIDTH);
        let x = area.x + (area.width - width) / 2;
        let row = |y: u16| Rect::new(x, y, width, 1);

        // Title, the two sources as tabs, and how fresh the feed is.
        let mut tx = x;
        Line::styled("TOP STORIES", t.muted().add_modifier(Modifier::BOLD))
            .render(row(area.y), buf);
        tx += 11 + 3;
        for source in Source::ALL {
            let label = format!(" {} ", source.label());
            let w = label.width() as u16;
            if tx + w > x + width {
                break;
            }
            let style = if source == self.source {
                t.on_accent_style().add_modifier(Modifier::BOLD)
            } else {
                t.muted()
            };
            let r = Rect::new(tx, area.y, w, 1);
            Line::styled(label, style).render(r, buf);
            self.hits.push((r, Click::Source(source)));
            tx += w + 1;
        }
        let feed = self.feeds.get(&self.source);
        let (status, status_style) = match feed {
            Some(f) if f.loading => ("loading…".to_string(), t.muted()),
            Some(Feed {
                error: Some(e),
                stories,
                ..
            }) if !stories.is_empty() => (
                format!("couldn't refresh: {}", first_line(e)),
                t.fg(t.error),
            ),
            Some(Feed {
                fetched: Some(at), ..
            }) => (
                match at.elapsed() {
                    d if d < Duration::from_secs(60) => "updated just now".into(),
                    d => format!("updated {} ago", short_duration(d)),
                },
                t.faint(),
            ),
            _ => (String::new(), Style::new()),
        };
        let room = (x + width).saturating_sub(tx + 2) as usize;
        if room > 8 {
            let status = truncate(&status, room);
            let w = status.width() as u16;
            Line::styled(status, status_style).render(Rect::new(x + width - w, area.y, w, 1), buf);
        }
        Line::styled("─".repeat(width as usize), t.faint()).render(row(area.y + 1), buf);

        let body_y = area.y + 3;
        let body_h = (area.y + area.height).saturating_sub(body_y);
        self.visible = usize::from(body_h / STORY_ROWS).max(1);
        let stories = feed.map(|f| f.stories.as_slice()).unwrap_or_default();
        if stories.is_empty() {
            let (msg, style) = match feed {
                Some(Feed { error: Some(e), .. }) if !feed.is_some_and(|f| f.loading) => (
                    format!(
                        "couldn't load {}: {}  ·  r to retry",
                        self.source.label(),
                        first_line(e)
                    ),
                    t.fg(t.error),
                ),
                _ => (format!("loading {}…", self.source.label()), t.muted()),
            };
            Line::styled(truncate(&msg, width as usize), style).render(row(body_y), buf);
            return;
        }

        // Keep the selection on the page.
        self.selected = self.selected.min(stories.len() - 1);
        self.top = self
            .top
            .min(stories.len().saturating_sub(self.visible))
            .min(self.selected);
        if self.selected >= self.top + self.visible {
            self.top = self.selected + 1 - self.visible;
        }

        for (slot, i) in (self.top..stories.len()).take(self.visible).enumerate() {
            let s = &stories[i];
            let y = body_y + slot as u16 * STORY_ROWS;
            let selected = i == self.selected;
            let (title_row, meta_row) = (row(y), row(y + 1));
            if selected {
                buf.set_style(title_row, t.selected());
                buf.set_style(meta_row, t.selected());
                for r in [title_row, meta_row] {
                    buf[(r.x, r.y)].set_symbol("▍").set_style(t.accent());
                }
            }

            // ` 1  Title                                   site.com`
            let num = format!("{:>3}  ", i + 1);
            let site = s.site().unwrap_or("");
            let title_room = (width as usize).saturating_sub(num.width() + site.width() + 3);
            let title = truncate(&s.title, title_room);
            let title_style = if selected {
                t.bold()
            } else {
                Style::new().fg(t.text)
            };
            let pad =
                (width as usize).saturating_sub(num.width() + title.width() + site.width() + 1);
            Line::from(vec![
                Span::styled(num.clone(), t.muted()),
                Span::styled(title, title_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(site.to_string(), t.faint()),
            ])
            .render(Rect::new(x + 1, y, width - 1, 1), buf);

            // `    592 points · 237 comments · 3h · by tuyuri · php security`
            let age = rough_age((now - s.posted).max(0) as u64);
            let mut meta = vec![
                Span::raw(" ".repeat(num.width())),
                Span::styled(count(s.points, "point"), t.muted()),
                Span::styled(" · ", t.faint()),
                Span::styled(count(s.comments, "comment"), t.muted()),
                Span::styled(" · ", t.faint()),
                Span::styled(age, t.muted()),
            ];
            if !s.by.is_empty() {
                meta.push(Span::styled(" · ", t.faint()));
                meta.push(Span::styled(format!("by {}", s.by), t.muted()));
            }
            if !s.tags.is_empty() {
                meta.push(Span::styled(" · ", t.faint()));
                meta.push(Span::styled(s.tags.join(" "), t.fg(t.git)));
            }
            Line::from(meta).render(Rect::new(x + 1, y + 1, width - 1, 1), buf);

            self.hits.push((title_row, Click::Open(i)));
            self.hits.push((meta_row, Click::Comments(i)));
        }
    }
}

/// `12m`, `18h`, `2d`: news doesn't need minutes on top of hours.
fn rough_age(secs: u64) -> String {
    match secs {
        0..=3599 => short_duration(Duration::from_secs(secs)),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `1 point`, `12 points`.
fn count(n: u64, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn story(n: u64) -> Story {
        Story {
            title: format!("Story number {n}"),
            url: Some(format!("https://www.example{n}.com/post?id={n}")),
            discussion: format!("https://news.ycombinator.com/item?id={n}"),
            points: 100 + n,
            comments: n,
            by: "pg".into(),
            posted: 1_000_000 - 3600 * n as i64,
            tags: Vec::new(),
        }
    }

    fn loaded(n: u64) -> Stories {
        let mut s = Stories::default();
        assert_eq!(s.want(false), Some(Source::HackerNews));
        s.loaded(Source::HackerNews, Ok((1..=n).map(story).collect()));
        s
    }

    fn render(s: &mut Stories, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| s.render(f.area(), f.buffer_mut(), 1_000_000))
            .unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn hacker_news_items() {
        let v: Value = serde_json::from_str(
            r#"{"by":"tuyuri","descendants":237,"id":50180873,"score":592,"time":1792590333,
                "title":"PHP\u001b[31m 8.2.35","type":"story","url":"https://github.com/php/php-src"}"#,
        )
        .unwrap();
        let s = parse_hacker_news_item(&v).unwrap();
        // The escape is gone; the rest is kept.
        assert_eq!(s.title, "PHP[31m 8.2.35");
        assert_eq!(s.site(), Some("github.com"));
        assert_eq!((s.points, s.comments), (592, 237));
        assert_eq!(
            s.discussion,
            "https://news.ycombinator.com/item?id=50180873"
        );

        // Ask HN: no link, so Enter goes to the comments.
        let v: Value =
            serde_json::from_str(r#"{"id":7,"title":"Ask HN: why?","score":3}"#).unwrap();
        let s = parse_hacker_news_item(&v).unwrap();
        assert_eq!(s.url, None);
        assert_eq!(s.link(), "https://news.ycombinator.com/item?id=7");

        let dead: Value = serde_json::from_str(r#"{"id":8,"title":"x","dead":true}"#).unwrap();
        assert_eq!(parse_hacker_news_item(&dead), None);
    }

    #[test]
    fn lobsters_feed() {
        let body = r#"[
            {"short_id":"ekxoyt","created_at":"2026-10-20T17:23:54.452-05:00",
             "title":"Upcoming Critical PHP Security Release",
             "url":"https://www.php.net/archive/2026.php#2026-10-20-1","score":139,
             "comment_count":86,"submitter_user":"pushcx","tags":["php","security"],
             "short_id_url":"https://lobste.rs/s/ekxoyt",
             "comments_url":"https://lobste.rs/s/ekxoyt/upcoming"},
            {"short_id":"abc","created_at":"2026-10-20T00:00:00Z","title":"A text post",
             "url":"","score":5,"comment_count":0,"submitter_user":{"username":"old"},
             "tags":[],"comments_url":"https://lobste.rs/s/abc/a"},
            {"title":"no comments page","url":"javascript:alert(1)"}
        ]"#;
        let s = parse_lobsters(body).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].site(), Some("php.net"));
        assert_eq!(s[0].tags, vec!["php", "security"]);
        assert_eq!(s[0].posted, 1_792_535_034);
        assert_eq!((s[1].url.as_deref(), s[1].by.as_str()), (None, "old"));
        assert!(parse_lobsters("[]").is_err());
    }

    #[test]
    fn times() {
        assert_eq!(unix_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_time("2000-03-01T00:00:00+00:00"), Some(951_868_800));
        assert_eq!(
            unix_time("2026-10-20T17:23:54.452-05:00"),
            Some(1_792_535_034)
        );
        assert_eq!(unix_time("yesterday"), None);
    }

    #[test]
    fn ages_are_rough() {
        assert_eq!(rough_age(42), "42s");
        assert_eq!(rough_age(65 * 60), "1h");
        assert_eq!(rough_age(18 * 3600 + 540), "18h");
        assert_eq!(rough_age(3 * 86_400), "3d");
    }

    #[test]
    fn only_web_links_open() {
        assert!(openable("https://example.com/a?b"));
        assert!(openable("HTTP://example.com"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("-a Calculator"));
    }

    #[test]
    fn page_snapshot() {
        let mut s = loaded(5);
        s.selected = 1;
        insta::assert_snapshot!(render(&mut s, 80, 14));
        // Title rows open the story, detail rows the comments.
        assert_eq!(s.click_at(10, 3), Some(Click::Open(0)));
        assert_eq!(s.click_at(10, 7), Some(Click::Comments(1)));
        assert!(matches!(
            s.click_at(30, 0),
            Some(Click::Source(Source::Lobsters))
        ));
    }

    #[test]
    fn loading_and_errors() {
        let mut s = Stories::default();
        s.want(false);
        assert!(render(&mut s, 60, 8).contains("loading Hacker News…"));
        // Already loading: not asked for twice.
        assert_eq!(s.want(true), None);
        s.loaded(Source::HackerNews, Err("offline".into()));
        assert!(render(&mut s, 80, 8).contains("couldn't load Hacker News: offline"));
        // Fresh feeds aren't fetched again unless asked.
        let mut s = loaded(3);
        assert_eq!(s.want(false), None);
        assert_eq!(s.want(true), Some(Source::HackerNews));
    }

    #[test]
    fn scrolling_keeps_the_selection_on_the_page() {
        let mut s = loaded(30);
        render(&mut s, 80, 3 + 3 * 5);
        assert_eq!(s.visible, 5);
        s.move_by(7);
        assert_eq!((s.selected, s.top), (7, 3));
        s.move_by(-5);
        assert_eq!((s.selected, s.top), (2, 2));
        // The wheel moves the page and drags the selection along.
        s.scroll_by(10);
        assert_eq!((s.selected, s.top), (12, 12));
        s.scroll_by(100);
        assert_eq!(s.top, 25);
        s.move_by(100);
        assert_eq!(s.selected, 29);
        // Switching sources starts at the top.
        s.switch(Source::Lobsters);
        assert_eq!((s.selected, s.top), (0, 0));
    }
}
