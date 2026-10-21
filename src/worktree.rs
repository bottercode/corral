//! Per-agent git worktrees, driven through the git CLI.
//!
//! A worktree lives outside the repository, under
//! `<root>/<repo-name>-<hash8>/<slug>`, on a branch `corral/<slug>` (or one
//! the user names), so parallel agents never share a checkout and the user's
//! own tree stays clean.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::protocol::WorktreeInfo;

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

fn fnv1a(s: &str) -> u64 {
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
    Ok(WorktreeInfo {
        path,
        branch,
        repo,
        changed_files: 0,
        pane: None,
    })
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
    Ok(WorktreeInfo {
        path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        branch,
        repo,
        changed_files: changed_files(path)?,
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
