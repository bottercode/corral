//! Per-agent git worktrees, driven through the git CLI.
//!
//! A worktree lives outside the repository, under
//! `<root>/<repo-name>-<hash8>/<slug>`, on a branch `corral/<slug>` (or one
//! the user names), so parallel agents never share a checkout and the user's
//! own tree stays clean.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::protocol::{DiffReport, FileDiff, WorktreeInfo};

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("{0} is not inside a git repository")]
    NotARepo(PathBuf),
    #[error("{path} has uncommitted changes ({files} {}); commit them or force removal", plural(*files, "file"))]
    Dirty { path: PathBuf, files: u32 },
    #[error(
        "branch {branch} has {commits} {} that aren't on any other branch; merge them or force deletion",
        plural(*commits, "commit")
    )]
    Unmerged { branch: String, commits: u32 },
    #[error("{0}")]
    Invalid(String),
    #[error("git {args}: {message}")]
    Git { args: String, message: String },
    #[error("running git: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// `file` / `files` for a count.
pub fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() {
        return Err(WorktreeError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The main working tree of the repository containing `dir` (also when
/// `dir` is itself inside a linked worktree).
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        return Err(WorktreeError::NotARepo(dir.to_path_buf()));
    }
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|e| match e {
        WorktreeError::Git { .. } => WorktreeError::NotARepo(dir.to_path_buf()),
        e => e,
    })?;
    let common = PathBuf::from(common);
    let root = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    } else {
        // Bare repository: no main working tree; use the git dir itself.
        common
    };
    Ok(root.canonicalize().unwrap_or(root))
}

pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Directory name for a repo's worktrees: `<name>-<hash8>`.
pub fn repo_dir_name(repo: &Path) -> String {
    let name = repo
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".into());
    format!("{name}-{:08x}", fnv1a(&repo.to_string_lossy()) as u32)
}

/// Lowercase, `[a-z0-9._-]` only, no leading/trailing separators.
pub fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(48)
        .collect()
}

fn random_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = fnv1a(&format!("{nanos}-{}", std::process::id()));
    format!("{:04x}", h as u16)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Create a worktree for an agent started in `dir`.
///
/// With `branch`, that branch is used (created from `HEAD` if it doesn't
/// exist yet); otherwise a fresh `corral/<hint>-<xxxx>` branch is made.
pub fn create(dir: &Path, root: &Path, branch: Option<&str>, hint: &str) -> Result<WorktreeInfo> {
    let repo = repo_root(dir)?;
    let (branch, slug) = match branch {
        Some(b) => {
            if git(&repo, &["check-ref-format", "--branch", b]).is_err() {
                return Err(WorktreeError::Invalid(format!(
                    "{b:?} is not a valid branch name"
                )));
            }
            let slug = sanitize(b.strip_prefix("corral/").unwrap_or(b));
            (
                b.to_string(),
                if slug.is_empty() { "wt".into() } else { slug },
            )
        }
        None => {
            let hint = match sanitize(hint) {
                h if h.is_empty() => "agent".to_string(),
                h => h,
            };
            let mut attempt = 0;
            loop {
                let slug = format!("{hint}-{}", random_suffix());
                let b = format!("corral/{slug}");
                if !branch_exists(&repo, &b) || attempt > 8 {
                    break (b, slug);
                }
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    };

    let parent = root.join(repo_dir_name(&repo));
    std::fs::create_dir_all(&parent)?;
    let mut path = parent.join(&slug);
    let mut n = 2;
    while path.exists() {
        path = parent.join(format!("{slug}-{n}"));
        n += 1;
    }
    let path_s = path.to_string_lossy().into_owned();
    let base = current_ref(dir)?;

    if branch_exists(&repo, &branch) {
        git(dir, &["worktree", "add", &path_s, &branch])?;
    } else {
        git(dir, &["worktree", "add", "-b", &branch, &path_s, "HEAD"]).map_err(|e| match e {
            WorktreeError::Git { message, .. } if message.contains("invalid reference: HEAD") => {
                WorktreeError::Invalid(format!(
                    "{} has no commits yet; make a first commit before using --worktree",
                    repo.display()
                ))
            }
            e => e,
        })?;
    }
    let path = path.canonicalize().unwrap_or(path);
    // Remember where the branch started, for diffs and merges later.
    let key = format!("branch.{branch}.corralBase");
    if git(&repo, &["config", "--get", &key]).is_err() {
        let _ = git(&repo, &["config", &key, &base]);
    }
    let base = base_of(&repo, &branch);
    Ok(WorktreeInfo {
        path,
        branch,
        repo,
        base: Some(base),
        ..Default::default()
    })
}

/// The branch checked out in `dir`, or the commit if HEAD is detached.
fn current_ref(dir: &Path) -> Result<String> {
    // symbolic-ref also works on a branch with no commits yet.
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .or_else(|_| git(dir, &["rev-parse", "HEAD"]))
}

/// Where `branch` started: recorded at creation, else the main checkout's
/// current branch.
pub fn base_of(repo: &Path, branch: &str) -> String {
    git(
        repo,
        &["config", "--get", &format!("branch.{branch}.corralBase")],
    )
    .or_else(|_| current_ref(repo))
    .unwrap_or_else(|_| "HEAD".into())
}

/// The commit the worktree's branch forked from its base.
fn fork_point(wt: &Path, base: &str) -> String {
    git(wt, &["merge-base", base, "HEAD"]).unwrap_or_else(|_| "HEAD".into())
}

const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;
const MAX_PATCH_BYTES: usize = 1024 * 1024;

fn untracked(wt: &Path) -> Result<Vec<String>> {
    let out = git(wt, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

/// Read an untracked file for display: `(text, binary)`; large files are
/// treated as binary.
fn read_untracked(wt: &Path, rel: &str) -> (Option<String>, bool) {
    let p = wt.join(rel);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_file() && m.len() <= MAX_UNTRACKED_BYTES => {}
        _ => return (None, true),
    }
    match std::fs::read(&p) {
        Ok(b) if !b.contains(&0) => (Some(String::from_utf8_lossy(&b).into_owned()), false),
        _ => (None, true),
    }
}

/// `+files`, `+lines`, `-lines` against the fork point, untracked files
/// included.
pub fn diff_stats(wt: &Path, base: &str) -> Result<(u32, u32, u32)> {
    let fork = fork_point(wt, base);
    let numstat = git(wt, &["diff", "--numstat", "--no-renames", &fork])?;
    let (mut files, mut added, mut removed) = (0u32, 0u32, 0u32);
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let a = parts
            .next()
            .and_then(|x| x.parse::<u32>().ok())
            .unwrap_or(0);
        let r = parts
            .next()
            .and_then(|x| x.parse::<u32>().ok())
            .unwrap_or(0);
        files += 1;
        added += a;
        removed += r;
    }
    for rel in untracked(wt)? {
        files += 1;
        if let (Some(text), false) = read_untracked(wt, &rel) {
            added += text.lines().count() as u32;
        }
    }
    Ok((files, added, removed))
}

/// Full diff of a worktree against where its branch started: committed and
/// uncommitted changes plus untracked files.
pub fn diff(wt: &Path) -> Result<DiffReport> {
    let info = info(wt)?;
    let base = info.base.clone().unwrap_or_else(|| "HEAD".into());
    let fork = fork_point(wt, &base);

    let mut files: Vec<FileDiff> = Vec::new();
    let status = git(wt, &["diff", "--name-status", "--no-renames", &fork])?;
    for line in status.lines() {
        if let Some((st, path)) = line.split_once('\t') {
            files.push(FileDiff {
                path: path.to_string(),
                status: st.chars().next().unwrap_or('M').to_string(),
                ..Default::default()
            });
        }
    }
    let numstat = git(wt, &["diff", "--numstat", "--no-renames", &fork])?;
    for line in numstat.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if let [a, r, path] = parts.as_slice()
            && let Some(f) = files.iter_mut().find(|f| f.path == *path)
        {
            f.binary = *a == "-";
            f.added = a.parse().unwrap_or(0);
            f.removed = r.parse().unwrap_or(0);
        }
    }
    let patch = git(
        wt,
        &["diff", "--no-color", "--no-renames", "--no-ext-diff", &fork],
    )?;
    for chunk in split_patch(&patch) {
        if let Some(f) = files.iter_mut().find(|f| f.path == chunk.0) {
            f.patch = truncate_patch(chunk.1);
        }
    }
    for rel in untracked(wt)? {
        let (text, binary) = read_untracked(wt, &rel);
        let mut f = FileDiff {
            path: rel,
            status: "?".into(),
            binary,
            ..Default::default()
        };
        if let Some(text) = text {
            let n = text.lines().count();
            f.added = n as u32;
            let mut p = format!("@@ -0,0 +1,{n} @@\n");
            for l in text.lines() {
                p.push('+');
                p.push_str(l);
                p.push('\n');
            }
            f.patch = truncate_patch(&p);
        }
        files.push(f);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let commits = git(wt, &["rev-list", "--count", &format!("{fork}..HEAD")])
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    Ok(DiffReport {
        pane: None,
        branch: info.branch,
        base,
        commits,
        uncommitted: info.changed_files,
        files,
    })
}

/// Split `git diff` output into `(path, hunks)` per file.
fn split_patch(patch: &str) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    let mut starts: Vec<usize> = patch
        .match_indices("diff --git ")
        .filter(|(i, _)| *i == 0 || patch.as_bytes()[i - 1] == b'\n')
        .map(|(i, _)| i)
        .collect();
    starts.push(patch.len());
    for w in starts.windows(2) {
        let section = &patch[w[0]..w[1]];
        let header = section.lines().next().unwrap_or("");
        // "diff --git a/<path> b/<path>"
        let path = header
            .rsplit_once(" b/")
            .map(|(_, p)| p.to_string())
            .unwrap_or_default();
        let hunks = section.find("\n@@").map_or("", |i| &section[i + 1..]);
        out.push((path, hunks));
    }
    out
}

fn truncate_patch(p: &str) -> String {
    if p.len() <= MAX_PATCH_BYTES {
        return p.to_string();
    }
    let mut cut = MAX_PATCH_BYTES;
    while !p.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… diff truncated ({} bytes) …\n", &p[..cut], p.len())
}

/// Commit everything in the worktree.
pub fn commit(wt: &Path, message: &str) -> Result<String> {
    if changed_files(wt)? == 0 {
        return Err(WorktreeError::Invalid("nothing to commit".into()));
    }
    let message = message.trim();
    if message.is_empty() {
        return Err(WorktreeError::Invalid("commit message is empty".into()));
    }
    git(wt, &["add", "-A"])?;
    git(wt, &["commit", "-q", "-m", message])?;
    let sha = git(wt, &["rev-parse", "--short", "HEAD"])?;
    Ok(format!("committed {sha}"))
}

/// Merge the worktree's branch into its base branch in the main checkout.
pub fn merge(wt: &Path) -> Result<String> {
    let info = info(wt)?;
    if info.changed_files > 0 {
        return Err(WorktreeError::Invalid(format!(
            "the worktree has uncommitted changes ({} {}); commit them first",
            info.changed_files,
            plural(info.changed_files, "file")
        )));
    }
    let base = info.base.clone().unwrap_or_default();
    let checked_out = current_ref(&info.repo)?;
    if checked_out != base {
        return Err(WorktreeError::Invalid(format!(
            "{} has {checked_out} checked out, not {base}; switch to {base} to merge",
            info.repo.display()
        )));
    }
    let dirty = git(
        &info.repo,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !dirty.trim().is_empty() {
        return Err(WorktreeError::Invalid(format!(
            "{} has uncommitted changes; commit or stash them before merging",
            info.repo.display()
        )));
    }
    let ahead: u32 = git(
        &info.repo,
        &["rev-list", "--count", &format!("{base}..{}", info.branch)],
    )?
    .trim()
    .parse()
    .unwrap_or(0);
    if ahead == 0 {
        return Err(WorktreeError::Invalid(format!(
            "nothing to merge: {} has no commits that {base} doesn't have",
            info.branch
        )));
    }
    let msg = format!("Merge {} (corral)", info.branch);
    if let Err(e) = git(&info.repo, &["merge", "--no-ff", "-m", &msg, &info.branch]) {
        let _ = git(&info.repo, &["merge", "--abort"]);
        return Err(WorktreeError::Invalid(format!(
            "merging {} into {base} conflicts; nothing was changed. Resolve it in the worktree (e.g. rebase onto {base}) and try again. ({e})",
            info.branch
        )));
    }
    Ok(format!(
        "merged {} into {base} ({ahead} {})",
        info.branch,
        plural(ahead, "commit")
    ))
}

/// Throw away uncommitted changes and untracked files in the worktree.
pub fn discard(wt: &Path) -> Result<String> {
    let n = changed_files(wt)?;
    if n == 0 {
        return Err(WorktreeError::Invalid("nothing to discard".into()));
    }
    git(wt, &["reset", "-q", "--hard", "HEAD"])?;
    git(wt, &["clean", "-fdq"])?;
    Ok(format!("discarded changes to {n} {}", plural(n, "file")))
}

/// Number of changed or untracked files (`git status --porcelain`).
pub fn changed_files(path: &Path) -> Result<u32> {
    let out = git(path, &["status", "--porcelain", "--untracked-files=normal"])?;
    Ok(out.lines().filter(|l| !l.trim().is_empty()).count() as u32)
}

/// Commits on `branch` that aren't on any other local or remote branch.
pub fn unmerged_commits(repo: &Path, branch: &str) -> Result<u32> {
    let out = git(
        repo,
        &[
            "rev-list",
            "--count",
            &format!("refs/heads/{branch}"),
            "--not",
            &format!("--exclude={branch}"),
            "--branches",
            "--remotes",
        ],
    )?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Describe an existing worktree.
pub fn info(path: &Path) -> Result<WorktreeInfo> {
    let repo = repo_root(path)?;
    let branch = git(path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let base = base_of(&repo, &branch);
    let (diff_files, added, removed) = diff_stats(path, &base).unwrap_or_default();
    Ok(WorktreeInfo {
        path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        branch,
        repo,
        base: Some(base),
        changed_files: changed_files(path)?,
        diff_files,
        added,
        removed,
        pane: None,
    })
}

/// Worktrees under `root` (`<root>/<repo-dir>/<slug>`).
pub fn list(root: &Path) -> Vec<WorktreeInfo> {
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(root) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = repos
        .flatten()
        .filter(|e| e.path().is_dir())
        .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.join(".git").is_file())
        .collect();
    dirs.sort();
    for d in dirs {
        if let Ok(i) = info(&d) {
            out.push(i);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOpts {
    pub force: bool,
    pub delete_branch: bool,
}

/// Refuse with a reason if removing would lose work.
pub fn check_removable(path: &Path, opts: RemoveOpts) -> Result<WorktreeInfo> {
    let wt = info(path)?;
    if opts.force {
        return Ok(wt);
    }
    if wt.changed_files > 0 {
        return Err(WorktreeError::Dirty {
            path: wt.path.clone(),
            files: wt.changed_files,
        });
    }
    if opts.delete_branch && wt.branch != "HEAD" {
        let commits = unmerged_commits(&wt.repo, &wt.branch)?;
        if commits > 0 {
            return Err(WorktreeError::Unmerged {
                branch: wt.branch.clone(),
                commits,
            });
        }
    }
    Ok(wt)
}

/// Remove a worktree (and optionally its branch).
pub fn remove(path: &Path, opts: RemoveOpts) -> Result<()> {
    let wt = check_removable(path, opts)?;
    let p = wt.path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if opts.force {
        args.push("--force");
    }
    args.push(&p);
    git(&wt.repo, &args)?;
    if opts.delete_branch && wt.branch != "HEAD" {
        git(&wt.repo, &["branch", "-D", &wt.branch])?;
    }
    if let Some(parent) = wt.path.parent() {
        // Only succeeds when empty.
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn run(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=corral-test",
                "-c",
                "user.email=test@corral.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    fn repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("My Repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        // Local identity so commit()/merge() work anywhere (CI included).
        run(&repo, &["config", "user.name", "corral-test"]);
        run(&repo, &["config", "user.email", "test@corral.invalid"]);
        run(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "init"]);
        let root = tmp.path().join("wt");
        (tmp, repo, root)
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("Feature/Fix Bug!"), "feature-fix-bug");
        assert_eq!(sanitize("--x--"), "x");
        assert_eq!(sanitize("claude"), "claude");
    }

    #[test]
    fn create_list_status_remove() {
        let (_tmp, repo, root) = repo();
        let wt = create(&repo, &root, None, "claude").unwrap();
        assert!(wt.branch.starts_with("corral/claude-"), "{}", wt.branch);
        assert!(wt.path.join("a.txt").exists());
        assert_eq!(wt.repo, repo.canonicalize().unwrap());
        let dir = wt
            .path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(dir.starts_with("my-repo-"), "{dir}");
        // The user's tree is untouched.
        assert_eq!(changed_files(&repo).unwrap(), 0);

        let listed = list(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].branch, wt.branch);

        std::fs::write(wt.path.join("b.txt"), "b").unwrap();
        std::fs::write(wt.path.join("a.txt"), "changed").unwrap();
        assert_eq!(changed_files(&wt.path).unwrap(), 2);

        let err = remove(&wt.path, RemoveOpts::default()).unwrap_err();
        assert!(
            matches!(err, WorktreeError::Dirty { files: 2, .. }),
            "{err}"
        );
        assert!(wt.path.exists());

        remove(
            &wt.path,
            RemoveOpts {
                force: true,
                delete_branch: false,
            },
        )
        .unwrap();
        assert!(!wt.path.exists());
        assert!(branch_exists(&repo, &wt.branch), "branch kept");
        assert!(list(&root).is_empty());
    }

    #[test]
    fn named_branch_and_branch_deletion() {
        let (_tmp, repo, root) = repo();
        let wt = create(&repo, &root, Some("feature/login"), "x").unwrap();
        assert_eq!(wt.branch, "feature/login");
        assert!(wt.path.ends_with("feature-login"));

        // A commit only on this branch is unmerged work.
        std::fs::write(wt.path.join("c.txt"), "c").unwrap();
        run(&wt.path, &["add", "."]);
        run(&wt.path, &["commit", "-qm", "work"]);
        let opts = RemoveOpts {
            force: false,
            delete_branch: true,
        };
        let err = remove(&wt.path, opts).unwrap_err();
        assert!(
            matches!(err, WorktreeError::Unmerged { commits: 1, .. }),
            "{err}"
        );

        // Removing just the worktree is fine: the commit stays on the branch.
        remove(&wt.path, RemoveOpts::default()).unwrap();
        assert!(branch_exists(&repo, "feature/login"));

        // Re-creating with the same branch checks it out again.
        let wt2 = create(&repo, &root, Some("feature/login"), "x").unwrap();
        assert!(wt2.path.join("c.txt").exists());
        remove(
            &wt2.path,
            RemoveOpts {
                force: true,
                delete_branch: true,
            },
        )
        .unwrap();
        assert!(!branch_exists(&repo, "feature/login"));
    }

    #[test]
    fn merged_branch_deletes_cleanly() {
        let (_tmp, repo, root) = repo();
        let wt = create(&repo, &root, None, "codex").unwrap();
        remove(
            &wt.path,
            RemoveOpts {
                force: false,
                delete_branch: true,
            },
        )
        .unwrap();
        assert!(!branch_exists(&repo, &wt.branch));
        // Empty repo dir is cleaned up.
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    }

    #[test]
    fn diff_commit_merge_discard() {
        let (_tmp, repo, root) = repo();
        let wt = create(&repo, &root, None, "claude").unwrap();
        assert_eq!(wt.base.as_deref(), Some("main"));

        // An edit, a new file and a binary file.
        std::fs::write(wt.path.join("a.txt"), "a\nchanged\n").unwrap();
        std::fs::write(wt.path.join("new.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.path.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let d = diff(&wt.path).unwrap();
        assert_eq!(d.base, "main");
        assert_eq!(d.uncommitted, 3);
        let names: Vec<(&str, &str)> = d
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![("a.txt", "M"), ("blob.bin", "?"), ("new.txt", "?")]
        );
        let a = &d.files[0];
        assert_eq!((a.added, a.removed), (1, 0));
        assert!(a.patch.starts_with("@@"), "{}", a.patch);
        assert!(a.patch.contains("+changed"));
        assert!(d.files[1].binary);
        assert!(d.files[2].patch.contains("+two"));
        assert_eq!(diff_stats(&wt.path, "main").unwrap(), (3, 3, 0));

        // Merging refuses uncommitted work; commit, then merge into main.
        assert!(
            merge(&wt.path)
                .unwrap_err()
                .to_string()
                .contains("commit them first")
        );
        assert!(
            commit(&wt.path, "agent work")
                .unwrap()
                .starts_with("committed")
        );
        assert_eq!(changed_files(&wt.path).unwrap(), 0);
        // Committed work still shows in the diff against the base.
        let d = diff(&wt.path).unwrap();
        assert_eq!((d.commits, d.uncommitted, d.files.len()), (1, 0, 3));
        let msg = merge(&wt.path).unwrap();
        assert!(msg.contains("into main (1 commit)"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(repo.join("new.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert!(
            merge(&wt.path)
                .unwrap_err()
                .to_string()
                .contains("nothing to merge")
        );

        // Discard drops uncommitted edits and untracked files.
        std::fs::write(wt.path.join("a.txt"), "oops").unwrap();
        std::fs::write(wt.path.join("junk.txt"), "x").unwrap();
        assert!(discard(&wt.path).unwrap().contains("2 files"));
        assert!(!wt.path.join("junk.txt").exists());
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "a\nchanged\n"
        );
        assert!(discard(&wt.path).is_err());
    }

    #[test]
    fn merge_refuses_when_checkout_is_elsewhere_or_conflicting() {
        let (_tmp, repo, root) = repo();
        let wt = create(&repo, &root, None, "x").unwrap();
        std::fs::write(wt.path.join("a.txt"), "from agent\n").unwrap();
        commit(&wt.path, "agent").unwrap();
        // Conflicting change on main.
        std::fs::write(repo.join("a.txt"), "from user\n").unwrap();
        run(&repo, &["commit", "-qam", "user"]);
        let err = merge(&wt.path).unwrap_err().to_string();
        assert!(err.contains("conflicts"), "{err}");
        // The user's checkout is left clean, not mid-merge.
        assert_eq!(changed_files(&repo).unwrap(), 0);
        assert!(!repo.join(".git/MERGE_HEAD").exists());

        run(&repo, &["checkout", "-qb", "other"]);
        let err = merge(&wt.path).unwrap_err().to_string();
        assert!(err.contains("has other checked out"), "{err}");
    }

    #[test]
    fn split_patch_by_file() {
        let p = "diff --git a/x y.txt b/x y.txt\nindex 1..2 100644\n--- a/x y.txt\n+++ b/x y.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/z b/z\nnew file mode 100644\n--- /dev/null\n+++ b/z\n@@ -0,0 +1 @@\n+z\n";
        let parts = split_patch(p);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "x y.txt");
        assert_eq!(parts[0].1, "@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(parts[1].0, "z");
    }

    #[test]
    fn not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let err = create(tmp.path(), &tmp.path().join("wt"), None, "x").unwrap_err();
        assert!(matches!(err, WorktreeError::NotARepo(_)), "{err}");
        assert!(err.to_string().contains("not inside a git repository"));
    }

    #[test]
    fn repo_without_commits() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-q"]);
        let err = create(tmp.path(), &tmp.path().join("wt"), None, "x").unwrap_err();
        assert!(err.to_string().contains("no commits yet"), "{err}");
    }

    #[test]
    fn invalid_branch_name() {
        let (_tmp, repo, root) = repo();
        let err = create(&repo, &root, Some("bad..name"), "x").unwrap_err();
        assert!(matches!(err, WorktreeError::Invalid(_)), "{err}");
    }

    #[test]
    fn worktree_of_a_worktree_uses_the_main_repo() {
        let (_tmp, repo, root) = repo();
        let a = create(&repo, &root, None, "a").unwrap();
        let b = create(&a.path, &root, None, "b").unwrap();
        assert_eq!(b.repo, a.repo);
        assert_eq!(b.path.parent(), a.path.parent());
    }
}
