//! `~/.config/corral/config.toml`.
//!
//! ```toml
//! prefix = "C-b"                    # C-a … C-z, or C-space
//! default_command = ""              # run in each new shell, e.g. "claude"
//! notifications = true              # bell + OSC 9 when an unfocused agent is blocked/done
//! sidebar_width = 30
//! sidebar_collapsed = false        # just agent numbers (^b c, or « in the sidebar)
//! theme = "ranch"                  # ranch, ink or terminal
//! sidebar_order = "need"           # need (what needs you first) or project
//! update_check = true              # once a day, a toast when a newer corral is out
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub prefix: String,
    pub default_command: String,
    pub notifications: bool,
    pub sidebar_width: u16,
    pub sidebar_collapsed: bool,
    pub theme: String,
    pub sidebar_order: String,
    pub update_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prefix: "C-b".into(),
            default_command: String::new(),
            notifications: true,
            sidebar_width: 30,
            sidebar_collapsed: false,
            theme: "ranch".into(),
            sidebar_order: "need".into(),
            update_check: true,
        }
    }
}

impl Config {
    /// Load from the default path; a missing file means defaults.
    pub fn load() -> Result<Self> {
        Self::load_from(&paths::config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&src).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(src: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(src)?;
        cfg.prefix_byte()?;
        crate::client::theme::by_name(&cfg.theme)?;
        if !SIDEBAR_ORDERS.contains(&cfg.sidebar_order.as_str()) {
            bail!(
                "sidebar_order must be one of {} (got {:?})",
                SIDEBAR_ORDERS.join(", "),
                cfg.sidebar_order
            );
        }
        Ok(cfg)
    }

    /// The byte the prefix key sends.
    pub fn prefix_byte(&self) -> Result<u8> {
        parse_prefix(&self.prefix)
    }
}

pub const SIDEBAR_ORDERS: [&str; 2] = ["need", "project"];

/// Set one top-level key in the config file, keeping everything else in it
/// (comments included). Creates the file if needed.
pub fn save(key: &str, value: impl Into<toml_edit::Value>) -> Result<()> {
    save_to(&paths::config_path(), key, value)
}

pub fn save_to(path: &Path, key: &str, value: impl Into<toml_edit::Value>) -> Result<()> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = src
        .parse()
        .with_context(|| format!("in {}", path.display()))?;
    doc[key] = toml_edit::value(value);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `C-b` → 0x02, `C-space` → 0x00.
pub fn parse_prefix(s: &str) -> Result<u8> {
    let lower = s.trim().to_ascii_lowercase();
    let key = lower
        .strip_prefix("c-")
        .or_else(|| lower.strip_prefix("ctrl-"))
        .or_else(|| lower.strip_prefix("ctrl+"))
        .or_else(|| lower.strip_prefix('^'));
    match key {
        Some("space") | Some("@") => Ok(0),
        Some(k) if k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase() => {
            Ok(k.as_bytes()[0] - b'a' + 1)
        }
        _ => bail!("prefix must look like C-b, C-a or C-space (got {s:?})"),
    }
}

/// Display form of a prefix byte, e.g. `^b`.
pub fn prefix_label(b: u8) -> String {
    match b {
        0 => "^space".into(),
        1..=26 => format!("^{}", (b'a' + b - 1) as char),
        _ => format!("0x{b:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.prefix_byte().unwrap(), 0x02);
        assert!(c.notifications);
        assert_eq!(c.default_command, "");
        assert_eq!(c.sidebar_order, "need");
    }

    #[test]
    fn prefixes() {
        assert_eq!(parse_prefix("C-a").unwrap(), 1);
        assert_eq!(parse_prefix("ctrl-b").unwrap(), 2);
        assert_eq!(parse_prefix("^z").unwrap(), 26);
        assert_eq!(parse_prefix("C-space").unwrap(), 0);
        assert!(parse_prefix("b").is_err());
        assert!(parse_prefix("C-1").is_err());
        assert_eq!(prefix_label(2), "^b");
    }

    #[test]
    fn full_file() {
        let c = Config::parse(
            "prefix = 'C-a'\ndefault_command = 'codex'\nnotifications = false\nsidebar_width = 24\ntheme = 'ink'\n",
        )
        .unwrap();
        assert_eq!(c.prefix_byte().unwrap(), 1);
        assert_eq!(c.sidebar_width, 24);
        assert_eq!(c.theme, "ink");
        assert!(Config::parse("theme = 'neon'").is_err());
        assert!(Config::parse("sidebar_order = 'random'").is_err());
        assert!(!c.notifications);
    }

    #[test]
    fn saving_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        save_to(&path, "theme", "ink").unwrap();
        assert_eq!(Config::load_from(&path).unwrap().theme, "ink");

        std::fs::write(&path, "# mine\nprefix = 'C-a'   # comfy\ntheme = 'ink'\n").unwrap();
        save_to(&path, "theme", "terminal").unwrap();
        save_to(&path, "notifications", false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# mine") && text.contains("# comfy"),
            "{text}"
        );
        let c = Config::load_from(&path).unwrap();
        assert_eq!((c.theme.as_str(), c.notifications), ("terminal", false));
        assert_eq!(c.prefix_byte().unwrap(), 1);
    }

    #[test]
    fn unknown_keys_and_bad_prefix_fail() {
        assert!(Config::parse("prefx = 'C-a'").is_err());
        assert!(Config::parse("prefix = 'x'").is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let c = Config::load_from(Path::new("/nonexistent/corral.toml")).unwrap();
        assert_eq!(c.prefix, "C-b");
    }
}
