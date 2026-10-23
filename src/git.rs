//! The branch a pane is on and its pull request, asked of `git` and `gh`.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::protocol::PullRequest;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The branch checked out in `dir`, or a short commit hash when `HEAD` is
/// detached. `None` outside a repository.
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .or_else(|| git(dir, &["rev-parse", "--short", "HEAD"]))
        .filter(|b| !b.is_empty())
}

/// The newest pull request whose head is `branch`, asked of GitHub with
/// `gh`. `None` when `gh` is missing or signed out, the repository isn't on
/// GitHub, or the branch has no pull request.
pub fn pull_request(dir: &Path, branch: &str) -> Option<PullRequest> {
    let out = Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "all", "--limit", "1",
        ])
        .args(["--json", "number,state"])
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let prs: Vec<PullRequest> = serde_json::from_slice(&out.stdout).ok()?;
    prs.into_iter().next()
}
