//! Session restore. The server keeps a record of its panes on disk (where
//! each runs, its worktree, and the agent in it with that agent's session),
//! rewritten whenever it changes and once more on the way down. A server
//! that starts and finds a record offers to bring the panes back: agents
//! resume their conversation, everything else comes back as a shell in the
//! same folder. Old commands are never re-run on their own.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::detect::agents::program_index;
use crate::paths;
use crate::protocol::Restorable;

const RECORD_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    pub version: u32,
    pub panes: Vec<Saved>,
}

/// One pane, as much as it takes to bring it back.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub name: String,
    /// The user chose the name.
    pub named: bool,
    pub workspace: String,
    pub cwd: PathBuf,
    /// Where it was started from: the repo, for worktree panes.
    pub origin: PathBuf,
    /// The worktree corral made for it.
    pub worktree: Option<PathBuf>,
    /// The agent running in it, if one was.
    pub agent: Option<SavedAgent>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SavedAgent {
    /// `claude`, `codex`, …
    pub name: String,
    /// Its command line, for the flags it ran with.
    pub argv: Vec<String>,
    /// Its conversation, when known.
    pub session: Option<String>,
}

/// How an agent picks up its conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// That exact session.
    Session(String),
    /// The latest one in its folder (it was the only one of its kind there).
    Latest,
    /// Let the agent ask (several shared a folder, none identified).
    Pick,
}

/// `~/.corral/sessions/<socket>-<hash>.json`: one record per server.
pub fn record_path(socket: &Path) -> PathBuf {
    let stem = socket
        .file_stem()
        .map(|s| crate::worktree::sanitize(&s.to_string_lossy()))
        .unwrap_or_default();
    let hash = crate::worktree::fnv1a(&socket.to_string_lossy()) as u32;
    paths::corral_home()
        .join("sessions")
        .join(format!("{stem}-{hash:08x}.json"))
}

pub fn load(path: &Path) -> Result<Option<Record>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let record: Record =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    Ok((record.version == RECORD_VERSION).then_some(record))
}

/// Write atomically, so a crash mid-write leaves the previous record.
pub fn save(path: &Path, record: &Record) -> Result<()> {
    let dir = path.parent().context("record path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

impl Record {
    pub fn new(panes: Vec<Saved>) -> Record {
        Record {
            version: RECORD_VERSION,
            panes,
        }
    }

    /// How the pane at `i` will resume: its session if known, else the
    /// latest in its folder if it was alone there, else the agent's picker.
    pub fn resume_for(&self, i: usize) -> Option<Resume> {
        let agent = self.panes[i].agent.as_ref()?;
        if let Some(id) = &agent.session {
            return Some(Resume::Session(id.clone()));
        }
        let alone = self
            .panes
            .iter()
            .filter(|p| p.cwd == self.panes[i].cwd)
            .filter(|p| p.agent.as_ref().is_some_and(|a| a.name == agent.name))
            .count()
            == 1;
        Some(if alone { Resume::Latest } else { Resume::Pick })
    }

    /// What a client is shown before deciding.
    pub fn summary(&self) -> Vec<Restorable> {
        (0..self.panes.len())
            .map(|i| {
                let p = &self.panes[i];
                let how = match (&p.agent, self.resume_for(i)) {
                    (Some(a), Some(r)) => match resume_argv(a, &r) {
                        Some(_) => match r {
                            Resume::Session(_) => "resumes its conversation",
                            Resume::Latest => "continues its latest conversation",
                            Resume::Pick => "asks which conversation",
                        },
                        None => "starts again",
                    },
                    _ => "shell",
                };
                Restorable {
                    name: p.name.clone(),
                    workspace: p.workspace.clone(),
                    agent: p.agent.as_ref().map(|a| a.name.clone()),
                    how: how.into(),
                    worktree: p.worktree.is_some(),
                }
            })
            .collect()
    }
}

/// The command line that brings an agent back; `None` for agents corral
/// doesn't know how to resume (they start again with the same flags).
pub fn resume_argv(agent: &SavedAgent, how: &Resume) -> Option<Vec<String>> {
    let flags = agent_flags(agent);
    let mut argv = vec![agent.name.clone()];
    match agent.name.as_str() {
        "claude" => {
            argv.extend(strip(
                &flags,
                &["-c", "--continue", "--fork-session"],
                &["-r", "--resume", "--session-id"],
            ));
            match how {
                Resume::Session(id) => argv.extend(["--resume".into(), id.clone()]),
                Resume::Latest => argv.push("--continue".into()),
                Resume::Pick => argv.push("--resume".into()),
            }
        }
        // Codex's flags go with the subcommand it ran; `resume` takes its own.
        "codex" => {
            argv.push("resume".into());
            match how {
                Resume::Session(id) => argv.push(id.clone()),
                Resume::Latest => argv.push("--last".into()),
                Resume::Pick => {}
            }
        }
        "opencode" => {
            argv.extend(strip(&flags, &["-c", "--continue"], &["-s", "--session"]));
            match how {
                Resume::Session(id) => argv.extend(["--session".into(), id.clone()]),
                Resume::Latest | Resume::Pick => argv.push("--continue".into()),
            }
        }
        _ => return None,
    }
    Some(argv)
}

/// The agent again, with the flags it had (for agents without resume).
pub fn restart_argv(agent: &SavedAgent) -> Vec<String> {
    let mut argv = vec![agent.name.clone()];
    argv.extend(agent_flags(agent));
    argv
}

/// What came after the agent's program on its command line (which may be
/// `node /path/to/claude --flag`).
fn agent_flags(agent: &SavedAgent) -> Vec<String> {
    match program_index(&agent.argv) {
        Some(i) => agent.argv[i + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// Drop `flags` and `with_value` flags (with their values) from `args`.
fn strip(args: &[String], flags: &[&str], with_value: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        if flags.contains(&a.as_str()) {
            continue;
        }
        if with_value.contains(&name) {
            // `--resume=id` carries it; `--resume id` has it next (and a bare
            // `--resume` has none).
            if !a.contains('=') && it.peek().is_some_and(|v| !v.starts_with('-')) {
                it.next();
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// One line for `$SHELL -lc`: each argument quoted when it needs to be.
pub fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- finding an agent's session ----------------------------------------

/// Files changed this long before the agent started still count (clocks
/// and the agent's own startup).
const SLACK: Duration = Duration::from_secs(5);

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// `~/.claude`, or `$CLAUDE_CONFIG_DIR`.
pub fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

/// `~/.codex`, or `$CODEX_HOME`.
pub fn codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

/// Claude keeps each folder's conversations in `projects/<folder>/`, the
/// folder's path with everything but letters and digits turned into `-`.
fn claude_project(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The Claude conversation written to most recently in `cwd` since
/// `started`: `<session id>.jsonl` in its project folder.
pub fn claude_session(dir: &Path, cwd: &Path, started: SystemTime) -> Option<String> {
    let project = dir.join("projects").join(claude_project(cwd));
    let since = started.checked_sub(SLACK).unwrap_or(started);
    std::fs::read_dir(project)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((modified(&p)?, p)))
        .filter(|(m, _)| *m >= since)
        .max_by_key(|(m, _)| *m)
        .and_then(|(_, p)| Some(p.file_stem()?.to_string_lossy().into_owned()))
}

/// The Codex session for `cwd` written to most recently since `started`.
/// Codex files them by day (`sessions/YYYY/MM/DD/rollout-….jsonl`), each
/// starting with a `session_meta` line that has the id and folder.
pub fn codex_session(dir: &Path, cwd: &Path, started: SystemTime) -> Option<String> {
    let since = started.checked_sub(SLACK).unwrap_or(started);
    // The newest few day folders are enough: the session was written since
    // the agent started.
    let mut days: Vec<PathBuf> = Vec::new();
    for year in subdirs(&dir.join("sessions")).into_iter().rev().take(2) {
        for month in subdirs(&year).into_iter().rev().take(2) {
            days.extend(subdirs(&month).into_iter().rev().take(3));
        }
    }
    days.sort();
    let mut files: Vec<(SystemTime, PathBuf)> = days
        .iter()
        .rev()
        .take(3)
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten())
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((modified(&p)?, p)))
        .filter(|(m, _)| *m >= since)
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().find_map(|(_, p)| {
        let (id, folder) = codex_meta(&p)?;
        (Path::new(&folder) == cwd).then_some(id)
    })
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

/// `(id, cwd)` from a Codex session file's first line.
fn codex_meta(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    // The first line carries the instructions too; it can be long.
    BufReader::new(file.take(4 << 20))
        .read_line(&mut line)
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    let meta = &v["payload"];
    Some((
        meta["id"].as_str()?.to_string(),
        meta["cwd"].as_str()?.to_string(),
    ))
}

/// The session of `agent` running in `cwd` since `started`, by the files it
/// writes (Claude and Codex).
pub fn find_session(agent: &str, cwd: &Path, started: SystemTime) -> Option<String> {
    match agent {
        "claude" => claude_session(&claude_dir(), cwd, started),
        "codex" => codex_session(&codex_dir(), cwd, started),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn agent(name: &str, argv: &[&str]) -> SavedAgent {
        SavedAgent {
            name: name.into(),
            argv: s(argv),
            session: None,
        }
    }

    #[test]
    fn claude_resumes_with_its_flags() {
        let a = agent(
            "claude",
            &[
                "node",
                "/x/claude",
                "--model",
                "opus",
                "--continue",
                "--resume",
                "old",
                "--verbose",
            ],
        );
        assert_eq!(
            resume_argv(&a, &Resume::Session("abc".into())).unwrap(),
            s(&["claude", "--model", "opus", "--verbose", "--resume", "abc"])
        );
        assert_eq!(
            resume_argv(&a, &Resume::Latest).unwrap(),
            s(&["claude", "--model", "opus", "--verbose", "--continue"])
        );
        let a = agent("claude", &["claude", "--session-id=1", "-r"]);
        assert_eq!(
            resume_argv(&a, &Resume::Pick).unwrap(),
            s(&["claude", "--resume"])
        );
    }

    #[test]
    fn other_agents() {
        let codex = agent("codex", &["codex", "--full-auto"]);
        assert_eq!(
            resume_argv(&codex, &Resume::Session("u-1".into())).unwrap(),
            s(&["codex", "resume", "u-1"])
        );
        assert_eq!(
            resume_argv(&codex, &Resume::Latest).unwrap(),
            s(&["codex", "resume", "--last"])
        );
        let oc = agent("opencode", &["/bin/opencode", "-c"]);
        assert_eq!(
            resume_argv(&oc, &Resume::Pick).unwrap(),
            s(&["opencode", "--continue"])
        );
        let amp = agent("amp", &["amp", "--x"]);
        assert_eq!(resume_argv(&amp, &Resume::Latest), None);
        assert_eq!(restart_argv(&amp), s(&["amp", "--x"]));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(
            shell_line(&s(&[
                "claude",
                "--append-system-prompt",
                "be brief",
                "it's"
            ])),
            "claude --append-system-prompt 'be brief' 'it'\\''s'"
        );
    }

    fn saved(cwd: &str, agent: Option<SavedAgent>) -> Saved {
        Saved {
            name: "x".into(),
            named: false,
            workspace: "w".into(),
            cwd: cwd.into(),
            origin: cwd.into(),
            worktree: None,
            agent,
        }
    }

    #[test]
    fn resume_picks_the_session_then_the_folder() {
        let mut known = agent("claude", &["claude"]);
        known.session = Some("s1".into());
        let r = Record::new(vec![
            saved("/a", Some(known)),
            saved("/a", Some(agent("claude", &["claude"]))),
            saved("/a", Some(agent("claude", &["claude"]))),
            saved("/b", Some(agent("claude", &["claude"]))),
            saved("/b", None),
        ]);
        assert_eq!(r.resume_for(0), Some(Resume::Session("s1".into())));
        // Three claudes in /a: no telling the other two apart.
        assert_eq!(r.resume_for(1), Some(Resume::Pick));
        assert_eq!(r.resume_for(3), Some(Resume::Latest));
        assert_eq!(r.resume_for(4), None);
        let how: Vec<String> = r.summary().into_iter().map(|x| x.how).collect();
        assert_eq!(
            how,
            vec![
                "resumes its conversation",
                "asks which conversation",
                "asks which conversation",
                "continues its latest conversation",
                "shell"
            ]
        );
    }

    #[test]
    fn records_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions/default-1.json");
        assert_eq!(load(&path).unwrap(), None);
        let r = Record::new(vec![saved("/a", Some(agent("codex", &["codex"])))]);
        save(&path, &r).unwrap();
        assert_eq!(load(&path).unwrap(), Some(r));
        // A record from another version is ignored, not misread.
        std::fs::write(&path, r#"{"version":99,"panes":[]}"#).unwrap();
        assert_eq!(load(&path).unwrap(), None);
    }

    #[test]
    fn claude_sessions_by_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/Users/me/.corral/worktrees/api-1/fix_it");
        let project = tmp
            .path()
            .join("projects/-Users-me--corral-worktrees-api-1-fix-it");
        std::fs::create_dir_all(&project).unwrap();
        let before = SystemTime::now() - Duration::from_secs(3600);
        std::fs::write(project.join("old.jsonl"), "{}").unwrap();
        // Older than the agent: not its session.
        let f = std::fs::File::options()
            .write(true)
            .open(project.join("old.jsonl"))
            .unwrap();
        f.set_modified(before).unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        assert_eq!(claude_session(tmp.path(), cwd, started), None);
        std::fs::write(project.join("abc-123.jsonl"), "{}").unwrap();
        assert_eq!(
            claude_session(tmp.path(), cwd, started).as_deref(),
            Some("abc-123")
        );
    }

    #[test]
    fn codex_sessions_by_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("sessions/2026/10/21");
        std::fs::create_dir_all(&day).unwrap();
        let meta = |id: &str, cwd: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"base_instructions\":{{\"text\":\"long\"}}}}}}\n{{}}\n"
            )
        };
        std::fs::write(day.join("rollout-a.jsonl"), meta("other", "/elsewhere")).unwrap();
        std::fs::write(day.join("rollout-b.jsonl"), meta("mine", "/repo")).unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        assert_eq!(
            codex_session(tmp.path(), Path::new("/repo"), started).as_deref(),
            Some("mine")
        );
        assert_eq!(codex_session(tmp.path(), Path::new("/nope"), started), None);
    }
}
