//! `corral integrate claude|codex`: let agents report their own state.
//!
//! Hooks run `corral report --hook` only inside corral panes (when
//! `$CORRAL_PANE` is set), so installing them globally is harmless for
//! agents started anywhere else. Changes are shown as a diff and confirmed
//! before writing; the original file is backed up once.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// Every hook command corral writes contains this, which is how its own
/// entries are recognised on reinstall and uninstall.
pub const MARKER: &str = "report --hook";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

pub fn hook_command(state: &str) -> String {
    format!("[ -z \"$CORRAL_PANE\" ] || \"${{CORRAL_BIN:-corral}}\" {MARKER} --state {state}")
}

/// Claude Code hook events and the state each one reports.
pub const CLAUDE_HOOKS: &[(&str, &str, Option<&str>)] = &[
    ("Notification", "blocked", None),
    ("Stop", "done", None),
    ("UserPromptSubmit", "working", None),
    ("PreToolUse", "working", Some("*")),
    ("PostToolUse", "working", Some("*")),
];

pub fn claude_settings_path() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("settings.json"),
        None => dirs::home_dir()
            .unwrap_or_default()
            .join(".claude")
            .join("settings.json"),
    }
}

pub fn codex_config_path() -> PathBuf {
    match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("config.toml"),
        None => dirs::home_dir()
            .unwrap_or_default()
            .join(".codex")
            .join("config.toml"),
    }
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(MARKER))
}

fn parse_settings(src: &str) -> Result<Map<String, Value>> {
    if src.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(src).context("settings.json is not valid JSON")? {
        Value::Object(m) => Ok(m),
        _ => bail!("settings.json must contain a JSON object"),
    }
}

/// Remove corral's hooks from a settings object, dropping empty leftovers.
fn strip_claude(settings: &mut Map<String, Value>) {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return;
    };
    for (_, groups) in hooks.iter_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for g in groups.iter_mut() {
            if let Some(Value::Array(list)) = g.get_mut("hooks") {
                list.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| match g.get("hooks") {
            Some(Value::Array(list)) => !list.is_empty(),
            _ => true,
        });
    }
    hooks.retain(|_, v| !matches!(v, Value::Array(a) if a.is_empty()));
    if hooks.is_empty() {
        settings.remove("hooks");
    }
}

fn to_pretty(settings: &Map<String, Value>) -> Result<String> {
    let mut s = serde_json::to_string_pretty(settings)?;
    s.push('\n');
    Ok(s)
}

pub fn claude_install(src: &str) -> Result<String> {
    let mut settings = parse_settings(src)?;
    strip_claude(&mut settings);
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        bail!("\"hooks\" in settings.json is not an object");
    };
    for (event, state, matcher) in CLAUDE_HOOKS {
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            bail!("hooks.{event} in settings.json is not an array");
        };
        let mut group = Map::new();
        if let Some(m) = matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": hook_command(state) }]),
        );
        groups.push(Value::Object(group));
    }
    to_pretty(&settings)
}

pub fn claude_uninstall(src: &str) -> Result<String> {
    let mut settings = parse_settings(src)?;
    strip_claude(&mut settings);
    to_pretty(&settings)
}

fn codex_notify() -> toml_edit::Array {
    let mut a = toml_edit::Array::new();
    a.push("sh");
    a.push("-c");
    a.push(hook_command("done"));
    // $0 for sh; Codex appends its JSON payload after this.
    a.push("corral-notify");
    a
}

fn codex_notify_is_ours(item: &toml_edit::Item) -> bool {
    item.as_array().is_some_and(|a| {
        a.iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains(MARKER)))
    })
}

pub fn codex_install(src: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = src.parse().context("config.toml is not valid TOML")?;
    if let Some(existing) = doc.get("notify")
        && !codex_notify_is_ours(existing)
    {
        bail!(
            "Codex already has `notify = {}` in config.toml; corral won't replace it. \
             Remove it or call `corral report --hook --state done` from your notify program.",
            existing.to_string().trim()
        );
    }
    doc["notify"] = toml_edit::value(codex_notify());
    Ok(doc.to_string())
}

pub fn codex_uninstall(src: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = src.parse().context("config.toml is not valid TOML")?;
    if doc.get("notify").is_some_and(codex_notify_is_ours) {
        doc.remove("notify");
    }
    Ok(doc.to_string())
}

/// Line diff with a little context, for showing changes before writing.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        return format!("--- old ({} lines)\n+++ new ({} lines)\n", a.len(), b.len());
    }
    // LCS table, then walk it.
    let mut t = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || t[i + 1][j] >= t[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    const CONTEXT: usize = 2;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (c, _))| *c != ' ')
        .map(|(k, _)| k)
        .collect();
    let mut out = String::new();
    let mut last_shown: Option<usize> = None;
    for (k, (c, line)) in ops.iter().enumerate() {
        let near = changed
            .iter()
            .any(|&ch| k + CONTEXT >= ch && k <= ch + CONTEXT);
        if !near {
            continue;
        }
        if last_shown.is_some_and(|l| k > l + 1) {
            out.push_str("  …\n");
        }
        out.push_str(&format!("{c} {line}\n"));
        last_shown = Some(k);
    }
    out
}

/// Apply `transform` to the file at `path` after showing the diff.
pub fn apply(path: &Path, transform: impl Fn(&str) -> Result<String>, yes: bool) -> Result<bool> {
    let old = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new = transform(&old)?;
    if new == old || (old.trim().is_empty() && new.trim() == "{}") {
        println!("{} is already up to date", path.display());
        return Ok(false);
    }
    println!("{}:\n{}", path.display(), diff(&old, &new));
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal; pass --yes to write without confirmation");
        }
        print!("Write these changes? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing written");
            return Ok(false);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !old.is_empty() {
        let backup = path.with_extension(format!(
            "{}.corral-backup",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        if !backup.exists() {
            std::fs::write(&backup, &old)
                .with_context(|| format!("writing backup {}", backup.display()))?;
            println!("backed up the original to {}", backup.display());
        }
    }
    let tmp = path.with_extension("corral-tmp");
    std::fs::write(&tmp, &new).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(true)
}

pub fn run_cli(agent: Agent, uninstall: bool, yes: bool) -> Result<()> {
    match (agent, uninstall) {
        (Agent::Claude, false) => {
            if apply(&claude_settings_path(), claude_install, yes)? {
                println!(
                    "Claude Code panes in corral now report blocked/working/done through hooks."
                );
            }
        }
        (Agent::Claude, true) => {
            apply(&claude_settings_path(), claude_uninstall, yes)?;
        }
        (Agent::Codex, false) => {
            if apply(&codex_config_path(), codex_install, yes)? {
                println!("Codex panes in corral now report done when a turn finishes.");
            }
        }
        (Agent::Codex, true) => {
            apply(&codex_config_path(), codex_uninstall, yes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_SETTINGS: &str = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "say done" }
        ]
      }
    ]
  },
  "permissions": { "allow": ["Bash(ls:*)"] }
}
"#;

    fn hooks(s: &str) -> Value {
        serde_json::from_str::<Value>(s).unwrap()["hooks"].clone()
    }

    #[test]
    fn claude_install_merges_with_user_hooks() {
        let out = claude_install(USER_SETTINGS).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls:*)");
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done");
        assert!(
            stop[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--state done")
        );
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert!(v["hooks"]["Notification"][0].get("matcher").is_none());
        // Key order is preserved.
        assert!(out.find("\"model\"").unwrap() < out.find("\"permissions\"").unwrap());
    }

    #[test]
    fn claude_install_is_idempotent() {
        let once = claude_install(USER_SETTINGS).unwrap();
        let twice = claude_install(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn claude_uninstall_restores_user_hooks() {
        let installed = claude_install(USER_SETTINGS).unwrap();
        let removed = claude_uninstall(&installed).unwrap();
        assert_eq!(hooks(&removed), hooks(USER_SETTINGS));
        assert!(!removed.contains(MARKER));
        // From an empty file, uninstall leaves no hooks key at all.
        let fresh = claude_install("").unwrap();
        let removed = claude_uninstall(&fresh).unwrap();
        assert_eq!(removed.trim(), "{}");
    }

    #[test]
    fn claude_rejects_bad_json() {
        assert!(claude_install("{ nope").is_err());
        assert!(claude_install("[1]").is_err());
    }

    #[test]
    fn hook_command_only_runs_in_corral() {
        let cmd = hook_command("blocked");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .env_remove("CORRAL_PANE")
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn codex_install_and_uninstall() {
        let src = "# my config\nmodel = \"o3\"\n\n[profiles.fast]\nmodel = \"mini\"\n";
        let out = codex_install(src).unwrap();
        assert!(out.starts_with("# my config"));
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        let notify = doc["notify"].as_array().unwrap();
        assert_eq!(notify.get(0).unwrap().as_str(), Some("sh"));
        assert!(doc["profiles"]["fast"].is_table());
        // notify stays a top-level key, before the first table.
        assert!(out.find("notify").unwrap() < out.find("[profiles.fast]").unwrap());
        assert_eq!(codex_install(&out).unwrap(), out);
        assert_eq!(codex_uninstall(&out).unwrap(), src);
    }

    #[test]
    fn codex_refuses_foreign_notify() {
        let src = "notify = [\"my-notifier\"]\n";
        let err = codex_install(src).unwrap_err().to_string();
        assert!(err.contains("won't replace it"), "{err}");
        assert_eq!(codex_uninstall(src).unwrap(), src);
    }

    #[test]
    fn apply_writes_backup_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, USER_SETTINGS).unwrap();
        assert!(apply(&path, claude_install, true).unwrap());
        assert!(tmp.path().join("settings.json.corral-backup").exists());
        assert!(!apply(&path, claude_install, true).unwrap());
        assert!(apply(&path, claude_uninstall, true).unwrap());
        assert_eq!(
            hooks(&std::fs::read_to_string(&path).unwrap()),
            hooks(USER_SETTINGS)
        );
    }

    #[test]
    fn diff_shows_changes_with_context() {
        let d = diff("a\nb\nc\nd\ne\nf\ng\n", "a\nb\nc\nX\ne\nf\ng\n");
        assert_eq!(d, "  b\n  c\n- d\n+ X\n  e\n  f\n");
    }
}
