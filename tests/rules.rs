//! Screen rules over recorded/reproduced agent screens.

mod common;

use std::time::{Duration, Instant};

use common::*;
use corral::detect::rules::{Rules, View};
use corral::detect::{Detector, Verdict};
use corral::protocol::AgentState;

fn classify(name: &str, agent: Option<&str>) -> Verdict {
    let path = format!(
        "{}/tests/fixtures/screens/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut p = vt100::Parser::new(50, 120, 0);
    p.process(&bytes);
    let text = p.screen().contents();
    let mut line = String::new();
    Rules::builtin().classify(agent, View::of_screen(p.screen(), &text, &mut line))
}

#[test]
fn claude_screens() {
    assert_eq!(
        classify("claude-trust.bytes", Some("claude")),
        Verdict::Blocked
    );
    assert_eq!(
        classify("claude-permission.bytes", Some("claude")),
        Verdict::Blocked
    );
    assert_eq!(
        classify("claude-working.bytes", Some("claude")),
        Verdict::Working
    );
    assert_eq!(classify("claude-idle.bytes", Some("claude")), Verdict::None);
}

#[test]
fn codex_screens() {
    assert_eq!(
        classify("codex-approval.bytes", Some("codex")),
        Verdict::Blocked
    );
    assert_eq!(
        classify("codex-working.bytes", Some("codex")),
        Verdict::Working
    );
}

#[test]
fn generic_screens() {
    assert_eq!(classify("spinner.bytes", None), Verdict::Working);
    assert_eq!(classify("yes-no.bytes", None), Verdict::Blocked);
    assert_eq!(classify("yes-no.bytes", Some("claude")), Verdict::Blocked);
    assert_eq!(classify("shell-prompt.bytes", None), Verdict::None);
    assert_eq!(classify("old-prompt-scrolled.bytes", None), Verdict::None);
}

#[test]
fn agent_specific_rules_dont_leak() {
    // Claude's trust prompt text in some other program isn't a blocked agent.
    assert_eq!(classify("claude-trust.bytes", Some("codex")), Verdict::None);
}

#[test]
fn fixture_through_the_state_machine() {
    let t0 = Instant::now();
    let mut d = Detector::new();
    d.on_output(t0);
    d.set_verdict(classify("claude-working.bytes", Some("claude")));
    assert_eq!(
        d.evaluate(t0 + Duration::from_millis(10)).map(|x| x.1),
        Some(AgentState::Working)
    );
    // Long after the last output, the spinner footer still says working.
    assert_eq!(d.evaluate(t0 + Duration::from_secs(30)), None);
    d.set_verdict(classify("claude-permission.bytes", Some("claude")));
    assert_eq!(
        d.evaluate(t0 + Duration::from_secs(31)).map(|x| x.1),
        Some(AgentState::Blocked)
    );
    d.set_verdict(classify("claude-idle.bytes", Some("claude")));
    assert_eq!(
        d.evaluate(t0 + Duration::from_secs(40)).map(|x| x.1),
        Some(AgentState::Done)
    );
}

#[test]
fn a_real_prompt_in_a_pane_is_detected_end_to_end() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--",
        "sh",
        "-c",
        "printf 'Overwrite? [y/N] '; read x; echo answered-$x; sleep 30",
    ]);
    assert_eq!(
        s.ok(&["wait", "p1", "--state", "blocked", "--timeout", "10"])
            .trim(),
        "blocked"
    );
    s.ok(&["send", "p1", "y", "--enter"]);
    eventually("answer", || s.ok(&["read", "p1"]).contains("answered-y"));
    // The answered prompt is still on screen, but the pane isn't blocked.
    eventually("no longer blocked", || {
        s.panes()[0].state != AgentState::Blocked
    });
}
