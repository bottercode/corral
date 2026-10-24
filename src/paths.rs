//! Filesystem locations: socket, state, logs and config.
//!
//! Every location can be overridden with an environment variable so tests
//! and multiple independent servers never collide:
//! `CORRAL_SOCKET`, `CORRAL_HOME` and `CORRAL_CONFIG`.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const SOCKET_ENV: &str = "CORRAL_SOCKET";
pub const HOME_ENV: &str = "CORRAL_HOME";
pub const CONFIG_ENV: &str = "CORRAL_CONFIG";
pub const PANE_ENV: &str = "CORRAL_PANE";
pub const BIN_ENV: &str = "CORRAL_BIN";

fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `$TMPDIR/corral-$UID/default.sock`, unless `override_` is set.
pub fn socket_path_from(
    override_: Option<OsString>,
    tmpdir: Option<OsString>,
    uid: u32,
) -> PathBuf {
    if let Some(p) = override_.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let tmp = tmpdir
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    tmp.join(format!("corral-{uid}")).join("default.sock")
}

pub fn socket_path() -> PathBuf {
    socket_path_from(
        std::env::var_os(SOCKET_ENV),
        std::env::var_os("TMPDIR"),
        uid(),
    )
}

/// Create the socket's directory with mode 0700 and refuse to use one that
/// belongs to someone else or is readable by others.
pub fn ensure_socket_dir(socket: &Path) -> Result<()> {
    let dir = socket
        .parent()
        .context("socket path has no parent directory")?;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        return Ok(());
    }
    let meta = std::fs::metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    if meta.uid() != uid() {
        bail!(
            "{} is owned by another user; point {SOCKET_ENV} at a socket inside a directory you own",
            dir.display()
        );
    }
    if meta.permissions().mode() & 0o077 != 0 && is_default_socket_dir(dir) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", dir.display()))?;
    }
    Ok(())
}

fn is_default_socket_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("corral-"))
}

/// `~/.corral`, or `CORRAL_HOME`.
pub fn corral_home() -> PathBuf {
    match std::env::var_os(HOME_ENV).filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home().join(".corral"),
    }
}

pub fn log_dir() -> PathBuf {
    corral_home().join("log")
}

/// Notes, one Markdown file each.
pub fn notes_dir() -> PathBuf {
    corral_home().join("notes")
}

/// The phone app's paired devices, push key and pairing code.
pub fn web_dir() -> PathBuf {
    corral_home().join("web")
}

/// `~/.config/corral/config.toml`, or `CORRAL_CONFIG`.
pub fn config_path() -> PathBuf {
    match std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home().join(".config").join("corral").join("config.toml"),
    }
}

/// Expand a leading `~/` to the home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_override_wins() {
        let p = socket_path_from(Some("/x/y.sock".into()), Some("/tmp".into()), 501);
        assert_eq!(p, PathBuf::from("/x/y.sock"));
    }

    #[test]
    fn socket_defaults_under_tmpdir_per_uid() {
        let p = socket_path_from(None, Some("/var/folders/T/".into()), 501);
        assert_eq!(p, PathBuf::from("/var/folders/T/corral-501/default.sock"));
        let p = socket_path_from(Some("".into()), None, 0);
        assert_eq!(p, PathBuf::from("/tmp/corral-0/default.sock"));
    }

    #[test]
    fn socket_dir_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("corral-test").join("default.sock");
        ensure_socket_dir(&sock).unwrap();
        let mode = std::fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        // An existing default-style dir that's too open gets tightened.
        std::fs::set_permissions(
            sock.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        ensure_socket_dir(&sock).unwrap();
        let mode = std::fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn tilde_expansion() {
        assert_eq!(expand_tilde("~/a/b"), home().join("a/b"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
        assert_eq!(expand_tilde("rel"), PathBuf::from("rel"));
    }
}
