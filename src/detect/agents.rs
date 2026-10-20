//! Which agent is running in a pane.
//!
//! The PTY's foreground process group leader is looked up with sysinfo and
//! matched by name and argv against the agent table. When the leader is a
//! shell (a command line run with `$SHELL -c`), its descendants are searched.

use std::collections::HashMap;
use std::path::Path;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "gemini",
    "amp",
    "cursor-agent",
    "copilot",
    "pi",
    "droid",
    "grok",
];

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "nu"];

fn stem(s: &str) -> String {
    let base = Path::new(s)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| s.to_string());
    let base = base.trim_start_matches('-').to_lowercase();
    for ext in [".exe", ".js", ".mjs", ".cjs"] {
        if let Some(b) = base.strip_suffix(ext) {
            return b.to_string();
        }
    }
    base
}

/// Match a process name and argv against the agent table. Runtimes like
/// node or bun put the agent's script in argv[1].
pub fn match_agent(name: &str, argv: &[String]) -> Option<&'static str> {
    let candidates = std::iter::once(name).chain(argv.iter().take(2).map(String::as_str));
    for c in candidates {
        let s = stem(c);
        if let Some(a) = AGENTS.iter().find(|a| **a == s) {
            return Some(a);
        }
    }
    None
}

pub fn is_shell(name: &str) -> bool {
    SHELLS.contains(&stem(name).as_str())
}

/// Resolves pane foreground processes to agent names.
pub struct Identifier {
    sys: System,
}

impl Default for Identifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Identifier {
    pub fn new() -> Self {
        Identifier { sys: System::new() }
    }

    /// Map each `(key, foreground pid)` to an agent name, if any.
    pub fn identify<K: Clone + Eq + std::hash::Hash>(
        &mut self,
        leaders: &[(K, u32)],
    ) -> HashMap<K, Option<&'static str>> {
        let kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet);
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let mut out = HashMap::new();
        for (key, pid) in leaders {
            out.insert(key.clone(), self.agent_for(*pid));
        }
        out
    }

    fn describe(&self, pid: Pid) -> Option<(String, Vec<String>)> {
        let p = self.sys.process(pid)?;
        let name = p.name().to_string_lossy().into_owned();
        let argv = p
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        Some((name, argv))
    }

    fn agent_for(&self, pid: u32) -> Option<&'static str> {
        let pid = Pid::from_u32(pid);
        let (name, argv) = self.describe(pid)?;
        if let Some(a) = match_agent(&name, &argv) {
            return Some(a);
        }
        if !is_shell(&name) {
            return None;
        }
        // Breadth-first over the shell's descendants.
        let mut frontier = vec![pid];
        for _ in 0..4 {
            let children: Vec<Pid> = self
                .sys
                .processes()
                .iter()
                .filter(|(_, p)| p.parent().is_some_and(|pp| frontier.contains(&pp)))
                .map(|(cpid, _)| *cpid)
                .collect();
            for c in &children {
                if let Some((n, a)) = self.describe(*c)
                    && let Some(agent) = match_agent(&n, &a)
                {
                    return Some(agent);
                }
            }
            if children.is_empty() {
                break;
            }
            frontier = children;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_native_binaries() {
        assert_eq!(match_agent("claude", &v(&["claude"])), Some("claude"));
        assert_eq!(
            match_agent("cursor-agent", &v(&["/usr/local/bin/cursor-agent"])),
            Some("cursor-agent")
        );
    }

    #[test]
    fn matches_node_scripts() {
        let argv = v(&[
            "node",
            "/Users/x/.nvm/versions/node/v22/bin/codex",
            "--full-auto",
        ]);
        assert_eq!(match_agent("node", &argv), Some("codex"));
        assert_eq!(
            match_agent("bun", &v(&["bun", "/opt/gemini.js"])),
            Some("gemini")
        );
    }

    #[test]
    fn ignores_other_programs() {
        assert_eq!(match_agent("vim", &v(&["vim", "claude.md"])), None);
        assert_eq!(
            match_agent("node", &v(&["node", "server.js", "codex"])),
            None
        );
    }

    #[test]
    fn shells() {
        assert!(is_shell("zsh"));
        assert!(is_shell("-zsh"));
        assert!(is_shell("/bin/bash"));
        assert!(!is_shell("claude"));
    }

    #[test]
    fn identifies_a_real_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let mut ident = Identifier::new();
        let got = ident.identify(&[("p1", child.id())]);
        assert_eq!(got.get("p1"), Some(&None));
        child.kill().ok();
        child.wait().ok();
    }
}
