//! Who may use the phone app. A phone pairs once with a one-time code (shown
//! as a QR code by `corral web`) and gets a device token in a cookie. Only
//! the token's SHA-256 is kept, in `~/.corral/web/devices.toml`.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long a pairing code works.
pub const PAIR_TTL: Duration = Duration::from_secs(10 * 60);
pub const COOKIE: &str = "corral_device";

/// Bytes from the system's random source.
pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(buf)
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compare without stopping at the first difference.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Create `dir` readable by this user only.
pub fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Write a file only this user can read, replacing it in one step.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(contents)?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    /// SHA-256 of the device's token, in hex.
    pub token: String,
    /// When it paired, in Unix seconds.
    pub paired: u64,
    /// Where to send its notifications, once it turns them on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    #[serde(default, rename = "device")]
    pub list: Vec<Device>,
}

impl Devices {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("devices.toml")
    }

    pub fn load(dir: &Path) -> Result<Devices> {
        let path = Self::path(dir);
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Devices::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        write_private(&Self::path(dir), toml::to_string(self)?.as_bytes())
    }

    /// The device this token belongs to.
    pub fn find(&self, token: &str) -> Option<&Device> {
        let h = hash(token);
        self.list
            .iter()
            .find(|d| same(d.token.as_bytes(), h.as_bytes()))
    }

    pub fn find_mut(&mut self, token: &str) -> Option<&mut Device> {
        let h = hash(token);
        self.list
            .iter_mut()
            .find(|d| same(d.token.as_bytes(), h.as_bytes()))
    }

    /// Add a device and return its token. Names stay unique ("Phone 2").
    pub fn add(&mut self, name: &str, now: SystemTime) -> Result<String> {
        let base: String = name
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect();
        let base = if base.is_empty() {
            "Phone".to_string()
        } else {
            base
        };
        let mut name = base.clone();
        let mut n = 2;
        while self.list.iter().any(|d| d.name == name) {
            name = format!("{base} {n}");
            n += 1;
        }
        let token = b64(&random::<32>()?);
        self.list.push(Device {
            name,
            token: hash(&token),
            paired: unix(now),
            push: None,
        });
        Ok(token)
    }

    pub fn revoke(&mut self, name: &str) -> bool {
        let before = self.list.len();
        self.list.retain(|d| d.name != name);
        self.list.len() != before
    }
}

/// The pairing code waiting to be used, kept in a file so `corral web pair`
/// can make a new one for a bridge that's already running.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Pairing {
    code: String,
    expires: u64,
}

fn pairing_path(dir: &Path) -> PathBuf {
    dir.join("pair.toml")
}

/// Make a new pairing code, replacing any earlier one.
pub fn new_pairing(dir: &Path, now: SystemTime) -> Result<String> {
    let code = b64(&random::<16>()?);
    let p = Pairing {
        code: code.clone(),
        expires: unix(now + PAIR_TTL),
    };
    write_private(&pairing_path(dir), toml::to_string(&p)?.as_bytes())?;
    Ok(code)
}

/// Use up the pairing code. False if it's wrong, used or expired.
pub fn redeem(dir: &Path, code: &str, now: SystemTime) -> bool {
    let path = pairing_path(dir);
    let Some(p) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<Pairing>(&s).ok())
    else {
        return false;
    };
    if unix(now) >= p.expires {
        let _ = std::fs::remove_file(&path);
        return false;
    }
    if !same(p.code.as_bytes(), code.as_bytes()) {
        return false;
    }
    std::fs::remove_file(&path).is_ok()
}

/// The device token in a `Cookie` header.
pub fn cookie_token(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
}

/// A request from another site carries an `Origin` that isn't one of ours.
/// `hosts` are the names this bridge answers to (`Host`, the forwarded host,
/// the `--url` one).
pub fn origin_allowed(origin: Option<&str>, hosts: &[&str]) -> bool {
    let Some(origin) = origin else {
        // Not sent by a browser page (the service worker's fetches are same-origin and
        // browsers always send Origin on WebSocket and POST).
        return false;
    };
    let authority = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .trim_end_matches('/');
    hosts
        .iter()
        .any(|h| !h.is_empty() && authority.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_pair_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = Devices::default();
        let t1 = d.add("iPhone", SystemTime::now()).unwrap();
        let t2 = d.add("iPhone", SystemTime::now()).unwrap();
        assert_ne!(t1, t2);
        assert_eq!(d.find(&t1).unwrap().name, "iPhone");
        assert_eq!(d.find(&t2).unwrap().name, "iPhone 2");
        assert!(d.find("nope").is_none());
        // Only the hash is stored.
        let saved = toml::to_string(&d).unwrap();
        assert!(!saved.contains(&t1));
        d.save(dir.path()).unwrap();
        let mut back = Devices::load(dir.path()).unwrap();
        assert_eq!(back, d);
        assert!(back.revoke("iPhone"));
        assert!(back.find(&t1).is_none());
        assert!(!back.revoke("iPhone"));
    }

    #[test]
    fn device_names_are_cleaned() {
        let mut d = Devices::default();
        d.add("  \x1b[31mx\n ", SystemTime::now()).unwrap();
        d.add("", SystemTime::now()).unwrap();
        let names: Vec<&str> = d.list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["[31mx", "Phone"]);
    }

    #[test]
    fn pairing_codes_work_once_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        assert!(!redeem(dir.path(), "x", now));
        let code = new_pairing(dir.path(), now).unwrap();
        assert!(!redeem(dir.path(), "wrong", now));
        assert!(redeem(dir.path(), &code, now));
        assert!(!redeem(dir.path(), &code, now), "single use");

        let code = new_pairing(dir.path(), now).unwrap();
        assert!(!redeem(dir.path(), &code, now + PAIR_TTL));

        // A new code replaces the old one.
        let old = new_pairing(dir.path(), now).unwrap();
        let new = new_pairing(dir.path(), now).unwrap();
        assert!(!redeem(dir.path(), &old, now));
        assert!(redeem(dir.path(), &new, now));
    }

    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let web = dir.path().join("web");
        new_pairing(&web, SystemTime::now()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&web), 0o700);
        assert_eq!(mode(&pairing_path(&web)), 0o600);
    }

    #[test]
    fn cookies() {
        assert_eq!(cookie_token("a=1; corral_device=tok; b=2"), Some("tok"));
        assert_eq!(cookie_token("corral_devicex=1"), None);
        assert_eq!(cookie_token(""), None);
    }

    #[test]
    fn origins() {
        let hosts = ["127.0.0.1:7777", "mac.tail.ts.net"];
        assert!(origin_allowed(Some("http://127.0.0.1:7777"), &hosts));
        assert!(origin_allowed(Some("https://MAC.tail.ts.net"), &hosts));
        assert!(!origin_allowed(Some("https://evil.example"), &hosts));
        assert!(!origin_allowed(
            Some("https://mac.tail.ts.net.evil.example"),
            &hosts
        ));
        assert!(!origin_allowed(None, &hosts));
        assert!(!origin_allowed(Some("null"), &["", "x"]));
    }
}
