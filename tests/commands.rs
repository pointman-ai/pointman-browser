//! The commands against the stand-in node and the fake CLI: the envelope back with a line about it, one session
//! per agent, a person's browser refused, a visible window only where it's allowed, screenshots posted, and calls
//! stopped at their deadline.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;
use pointman_extension::testing::RunOptions;
use serde_json::json;

#[test]
fn a_call_answers_its_envelope_and_a_line() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("browser.run", json!({"args": ["open", "https://example.com"], "agent": "@patchbay"})).unwrap();
    let r = &done.result;
    assert_eq!(r["ok"], true);
    assert_eq!(r["success"], true, "agent-browser's own field stays");
    assert_eq!(r["data"]["title"], "Example Domain");
    assert_eq!(r["text"], "open: Example Domain (https://example.com)");
    assert_eq!(r["machine"], "testbox");
    assert_eq!(r["session"], "patchbay");

    let snap = node.run("browser.run", json!({"args": ["snapshot", "-i", "-c"], "agent": "patchbay"})).unwrap();
    assert_eq!(snap.result["text"], "snapshot: 1 refs on https://example.com/");

    let call = &fake.calls_of("open")[0];
    let home = node.state_dir_path().join("home");
    assert_eq!(std::path::Path::new(&call.home), home, "the browser's home is the extension's own, not the person's");
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(call.sockets.starts_with("/tmp/pbr-"), "{}", call.sockets);
    let cwd = std::path::Path::new(&call.cwd).canonicalize().unwrap();
    assert!(cwd.starts_with(node.work_dir().canonicalize().unwrap()), "it runs in the job's folder: {}", call.cwd);
    for c in fake.calls() {
        assert_eq!(c.leak, "<none>", "the node's own AGENT_BROWSER_ variables never reach it");
        assert_eq!(&c.args[..3], ["--session", "patchbay", "--json"], "{:?}", c.args);
    }
}

#[test]
fn every_agent_has_its_own_session() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    node.run("browser.run", json!({"args": ["open", "https://a.test"], "agent": "patchbay"})).unwrap();
    node.run("browser.run", json!({"args": ["open", "https://b.test"], "agent": "@deskprobe"})).unwrap();
    node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay"})).unwrap();
    let sessions: Vec<_> = fake.calls().iter().map(|c| c.flag("--session").unwrap()).collect();
    assert_eq!(sessions, ["patchbay", "deskprobe", "patchbay"]);
    let e = node.run("browser.run", json!({"args": ["snapshot"]}));
    assert!(e.is_err(), "agent is required");
}

#[test]
fn a_persons_browser_and_other_sessions_are_refused() {
    let fake = Fake::new();
    let node = node(&fake, json!({"headed": true}));
    for args in [
        json!(["--profile", "Default", "open", "https://x.test"]),
        json!(["--auto-connect", "snapshot"]),
        json!(["--cdp", "9222", "snapshot"]),
        json!(["--session", "someone-else", "snapshot"]),
        json!(["profiles"]),
        json!(["connect", "9222"]),
        json!(["open", "file:///etc/passwd"]),
        json!(["upload", "@e1", "/etc/passwd"]),
        json!(["clipboard", "read"]),
        json!(["close", "--all"]),
        json!(["wait", "--download", "/tmp/x"]),
        json!(["screenshot", "/Users/someone/Desktop/a.png"]),
        json!(["screenshot", "../a.png"]),
        json!(["open", "https://x.test", "--allow-file-access"]),
    ] {
        let e = node.run("browser.run", json!({"args": args, "agent": "a"})).unwrap_err().to_string();
        assert!(e.contains("isn't allowed"), "{args}: {e}");
    }
    assert!(fake.calls().is_empty(), "nothing ran");
}

#[test]
fn a_visible_window_only_where_its_allowed() {
    let fake = Fake::new();
    let off = node(&fake, json!({}));
    let e = off.run("browser.run", json!({"args": ["--headed", "open", "https://x.test"], "agent": "a"})).unwrap_err();
    assert!(e.to_string().contains("off on testbox"), "{e}");
    assert!(fake.calls().is_empty());
    for spelled in
        [json!(["--headed=true", "open", "https://x.test"]), json!(["open", "https://x.test", "--headed", "true"])]
    {
        let e = off.run("browser.run", json!({"args": spelled, "agent": "a"})).unwrap_err().to_string();
        assert!(e.contains("visible browser window is off"), "{spelled}: {e}");
    }
    let on = common::node(&fake, json!({"headed": true}));
    on.run("browser.run", json!({"args": ["--headed", "open", "https://x.test"], "agent": "a"})).unwrap();
    assert!(fake.calls_of("open")[0].args.contains(&"--headed".to_string()));
}

#[test]
fn a_screenshot_is_posted_not_answered() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("browser.run", json!({"args": ["screenshot"], "agent": "a"})).unwrap();
    assert_eq!(done.result["data"]["file"], "screenshot-1.png");
    assert_eq!(done.outputs.len(), 1);
    assert_eq!(done.outputs[0].kind, "image");
    assert!(std::fs::read(&done.outputs[0].path).unwrap().ends_with(b"DATA"));
    assert!(done.result["text"].as_str().unwrap().ends_with("posted screenshot-1.png"));

    let to_file = node.run("browser.run", json!({"args": ["screenshot", "out.png"], "agent": "a"})).unwrap();
    assert_eq!(to_file.outputs.len(), 1, "a file it wrote where it ran is posted too");
    assert!(to_file.outputs[0].path.ends_with("out.png"), "{:?}", to_file.outputs[0]);
}

#[test]
fn a_huge_answer_goes_back_as_a_file() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("browser.run", json!({"args": ["eval", "big"], "agent": "a"})).unwrap();
    assert_eq!(done.result["data"]["file"], "eval.json");
    assert!(done.result.to_string().len() < 2000);
    assert_eq!(done.outputs[0].kind, "document");
}

#[test]
fn errors_are_answers_and_crashes_are_failures() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let done = node.run("browser.run", json!({"args": ["click", "@nope"], "agent": "a"})).unwrap();
    assert_eq!(done.result["ok"], false);
    assert_eq!(done.result["error"]["message"], "Element not found: @nope");
    assert_eq!(done.result["text"], "click: Element not found: @nope");
    let e = node.run("browser.run", json!({"args": ["eval", "crash"], "agent": "a"})).unwrap_err().to_string();
    assert!(e.contains("without its JSON (exit 101)") && e.contains("panicked"), "{e}");
    let e = node.run("browser.run", json!({"args": [], "agent": "a"})).unwrap_err().to_string();
    assert!(e.contains("args is empty"), "{e}");
}

#[test]
fn a_slow_call_is_stopped_at_its_deadline() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let started = std::time::Instant::now();
    let options = RunOptions { timeout: Duration::from_secs(20), ..Default::default() };
    let e =
        node.run_with("browser.run", json!({"args": ["wait", "8"], "agent": "a", "timeout": 1}), options).unwrap_err();
    assert!(e.to_string().contains("didn't answer in time"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
}

#[test]
fn a_chrome_setting_reaches_the_cli() {
    let fake = Fake::new();
    let node = node(&fake, json!({"chrome": "/opt/chromium/chrome"}));
    node.run("browser.run", json!({"args": ["snapshot"], "agent": "a"})).unwrap();
    assert_eq!(fake.calls_of("snapshot")[0].chrome, "/opt/chromium/chrome");
    let s = node.run("browser.status", json!({})).unwrap();
    assert_eq!(s.result["data"]["chrome"]["kind"], "system");
}

#[test]
fn status_and_health_say_what_is_here() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let s = node.run("browser.status", json!({})).unwrap();
    assert_eq!(s.result["ok"], true);
    assert_eq!(s.result["data"]["version"], "9.9.9");
    assert_eq!(s.result["data"]["sessions"], json!(["patchbay", "deskprobe"]));
    assert_eq!(s.result["data"]["headed_allowed"], false);
    assert!(s.result["text"].as_str().unwrap().starts_with("agent-browser 9.9.9 on testbox"), "{}", s.result["text"]);
    assert_eq!(part(&node, "cli")["state"], "STATE_OK");
    assert_eq!(part(&node, "chrome")["state"], "STATE_OK");
}

#[test]
fn a_handle_with_nothing_left_has_no_session() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    for agent in ["@", "///", " ", ""] {
        let e = node.run("browser.run", json!({"args": ["snapshot"], "agent": agent})).unwrap_err().to_string();
        assert!(e.contains("agent is your handle"), "{agent:?}: {e}");
    }
    assert!(fake.calls().is_empty(), "nothing ran");
}
