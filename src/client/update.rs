//! A once-a-day look at the latest GitHub release, so the client can show a
//! toast when a newer corral is out. The last check is remembered in
//! `~/.corral/update.json`; until a day has passed nothing is fetched and no
//! toast comes back, so attaching again doesn't repeat it.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths;

const LATEST: &str = "https://api.github.com/repos/bottercode/corral/releases/latest";
pub const INSTALL: &str = "curl -fsSL https://bottercode.github.io/corral/install.sh | sh";
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Checked {
    /// Unix seconds.
    at: u64,
    latest: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The newer version, if one is out and a day has passed since the last
/// check. Blocking: the client runs it on a worker thread.
pub fn check() -> Result<Option<String>> {
    check_in(
        &paths::corral_home().join("update.json"),
        now(),
        fetch_latest,
    )
}

fn check_in(
    file: &Path,
    now: u64,
    fetch: impl FnOnce() -> Result<String>,
) -> Result<Option<String>> {
    let last: Checked = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if now.saturating_sub(last.at) < EVERY.as_secs() {
        return Ok(None);
    }
    let latest = fetch()?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let checked = Checked { at: now, latest };
    std::fs::write(file, serde_json::to_string(&checked)?)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(newer(&checked.latest, env!("CARGO_PKG_VERSION")).then_some(checked.latest))
}

fn fetch_latest() -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("corral/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let body = agent
        .get(LATEST)
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .with_context(|| format!("fetching {LATEST}"))?;
    let v: Value = serde_json::from_str(&body).context("reading the latest release")?;
    let tag = v["tag_name"]
        .as_str()
        .context("the latest release has no tag")?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// `1.2.3` → (1, 2, 3); anything else (pre-releases included) is `None`.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').split('.');
    let n = |p: Option<&str>| p?.parse().ok();
    let version = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(version)
}

/// `latest` is a release later than `current`.
pub fn newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.1.3", "0.1.2"));
        assert!(newer("v0.2.0", "0.1.9"));
        assert!(newer("1.0.0", "0.10.10"));
        assert!(!newer("0.1.2", "0.1.2"));
        assert!(!newer("0.1.1", "0.1.2"));
        assert!(!newer("0.2.0-rc.1", "0.1.2"));
        assert!(!newer("garbage", "0.1.2"));
    }

    #[test]
    fn checks_at_most_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("home/update.json");
        let day = EVERY.as_secs();
        let found = check_in(&file, day, || Ok("999.0.0".into())).unwrap();
        assert_eq!(found.as_deref(), Some("999.0.0"));
        // Within the day: no fetch, no toast.
        let again = check_in(&file, day + 60, || panic!("fetched again")).unwrap();
        assert_eq!(again, None);
        // A day later it looks again; the same version is still newer.
        let later = check_in(&file, 2 * day, || Ok("999.0.0".into())).unwrap();
        assert_eq!(later.as_deref(), Some("999.0.0"));
        // Up to date: nothing to say.
        let current = check_in(&file, 3 * day, || Ok(env!("CARGO_PKG_VERSION").into())).unwrap();
        assert_eq!(current, None);
    }

    #[test]
    fn a_failed_fetch_tries_again_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("update.json");
        assert!(check_in(&file, EVERY.as_secs(), || anyhow::bail!("offline")).is_err());
        assert!(!file.exists());
    }
}
