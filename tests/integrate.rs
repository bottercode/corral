//! `corral integrate` through the binary, against temp config dirs.

mod common;

use std::process::{Command, Stdio};

use common::BIN;

fn run(args: &[&str], env: (&str, &std::path::Path)) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .env(env.0, env.1)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn claude_install_uninstall_via_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = tmp.path().join("settings.json");
    std::fs::write(&settings, "{\n  \"model\": \"opus\"\n}\n").unwrap();
    let env = ("CLAUDE_CONFIG_DIR", tmp.path());

    // Without a terminal and without --yes it refuses to write.
    let out = run(&["integrate", "claude"], env);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(
        !std::fs::read_to_string(&settings)
            .unwrap()
            .contains("hooks")
    );

    let out = run(&["integrate", "claude", "--yes"], env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("+ "), "diff shown: {stdout}");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["model"], "opus");
    for event in [
        "Notification",
        "Stop",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
    ] {
        assert!(v["hooks"][event].is_array(), "{event}");
    }
    assert!(tmp.path().join("settings.json.corral-backup").exists());

    let out = run(&["integrate", "claude", "--yes"], env);
    assert!(String::from_utf8_lossy(&out.stdout).contains("already up to date"));

    let out = run(&["integrate", "claude", "--uninstall", "--yes"], env);
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(v.get("hooks").is_none(), "{v}");
}

#[test]
fn codex_install_via_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let env = ("CODEX_HOME", tmp.path());
    // A missing config file is created.
    let out = run(&["integrate", "codex", "--yes"], env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("notify = [\"sh\", \"-c\""), "{text}");
    let out = run(&["integrate", "codex", "--uninstall", "--yes"], env);
    assert!(out.status.success());
    assert!(!std::fs::read_to_string(&config).unwrap().contains("notify"));
}
