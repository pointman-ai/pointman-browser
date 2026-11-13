//! By hand only, online: `cargo test --test live -- --ignored --nocapture`. The extension installs the real pinned
//! agent-browser and its Chrome into a scratch state folder, then opens example.com, snapshots, and takes a still.

use pointman_extension::testing::StandIn;
use serde_json::json;

#[test]
#[ignore]
fn drives_a_real_page() {
    let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_browser"))
        .settings(json!({}))
        .machine("live")
        .start()
        .unwrap();
    let run = |args: serde_json::Value| node.run("browser.run", json!({"args": args, "agent": "live-test"})).unwrap();
    let open = run(json!(["open", "https://example.com"]));
    println!("{}", open.result["text"]);
    assert_eq!(open.result["ok"], true, "{}", open.result);
    let snap = run(json!(["snapshot", "-i", "-c"]));
    println!("{}", snap.result["text"]);
    assert_eq!(snap.result["ok"], true, "{}", snap.result);
    let shot = run(json!(["screenshot"]));
    println!("{}", shot.result["text"]);
    assert_eq!(shot.outputs.len(), 1);
    run(json!(["close"]));
}

/// A checklist run on a real page; the report is written to $REPORT_OUT (default /tmp/browser-report.html).
#[test]
#[ignore]
fn reports_a_real_run() {
    let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))
        .binary(env!("CARGO_BIN_EXE_browser"))
        .settings(json!({}))
        .machine("live")
        .start()
        .unwrap();
    let run = |args: serde_json::Value| node.run("browser.run", json!({"args": args, "agent": "live-test"})).unwrap();
    let start = node
        .run(
            "browser.test_start",
            json!({"name": "example.com", "agent": "live-test",
            "checklist": "- Open the page\n  expect: the title is Example Domain\n- Find the Learn more link\n  expect: a link named Learn more\n- Find a Sign in button\n  expect: a Sign in button"}),
        )
        .unwrap();
    let id = start.result["run"].as_str().unwrap().to_string();
    run(json!(["open", "https://example.com"]));
    node.run("browser.test_step", json!({"run": id, "step": 1, "ok": true, "note": "Title: Example Domain"})).unwrap();
    let snap = run(json!(["snapshot", "-i", "-c"]));
    let has_link = snap.result["data"]["snapshot"].as_str().unwrap_or("").contains("Learn more");
    node.run(
        "browser.test_step",
        json!({"run": id, "step": 2, "ok": has_link, "note": "link Learn more in the snapshot"}),
    )
    .unwrap();
    node.run("browser.test_step", json!({"run": id, "step": 3, "ok": false, "note": "The page has no Sign in button"}))
        .unwrap();
    let done = node.run("browser.test_finish", json!({"run": id})).unwrap();
    println!("{}", done.result["text"]);
    let report = done.outputs.iter().find(|o| o.kind == "document").unwrap();
    let to = std::env::var("REPORT_OUT").unwrap_or_else(|_| "/tmp/browser-report.html".into());
    std::fs::copy(&report.path, &to).unwrap();
    println!("report: {to}");
    run(json!(["close"]));
    assert_eq!(done.result["failed"], 1);
}

/// Sign in yourself on a real page, by hand: `cargo test --test live signs_in -- --ignored --nocapture`, on nixos
/// (never on mac-m4). The pane's frames come from agent-browser's real stream; Tab then Enter through Control follow
/// example.com's link; Save keeps that origin; then a cookie set in the pane's session reaches an agent's run through
/// STATE, and a hidden value is typed on stdin on its own site only.
#[test]
#[ignore]
fn signs_in_from_the_pane_and_a_run_uses_it() {
    use std::time::Duration;
    let chrome = std::env::var("LIVE_CHROME").unwrap_or_default();
    let settings = if chrome.is_empty() { json!({}) } else { json!({"chrome": chrome}) };
    let state = tempfile::tempdir().unwrap();
    let start = |secrets: Vec<(String, String)>| {
        StandIn::new(env!("CARGO_MANIFEST_DIR"))
            .binary(env!("CARGO_BIN_EXE_browser"))
            .settings(settings.clone())
            .state_dir(state.path())
            .machine("live")
            .secrets(secrets)
            .start()
            .unwrap()
    };
    let node = start(vec![]);
    let ask = |id: &str, url: &str| {
        node.call(
            "credentials.sign_in",
            json!({"form": "login", "label": "live", "person": {"name": "james"}, "session": {"id": id}, "start": url}),
        )
        .unwrap()
    };

    // 1. frames from the real stream, and keys through Control
    let answer = ask("live_1", "https://example.com/");
    println!("sign_in: {answer}");
    let pane = node.open_pane("signin", json!({"session": "live_1"})).unwrap();
    pane.viewers(1).unwrap();
    pane.wait(Duration::from_secs(30), |p| p.frame_count() > 0).unwrap();
    let f = &pane.video_frames()[0];
    println!("first frame: {}x{}, {} bytes", f.width, f.height, f.jpeg.len());
    assert!(f.width >= 320 && f.height >= 180 && f.jpeg.starts_with(&[0xFF, 0xD8]));
    let until = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    pane.control(Some(until), "james", "live").unwrap();
    pane.control_input(json!({"kind": "pointer", "x": 0.5, "y": 0.9, "phase": "move"})).unwrap();
    pane.control_input(json!({"kind": "key", "code": 48})).unwrap(); // Tab: to the page's one link
    pane.control_input(json!({"kind": "key", "code": 36})).unwrap(); // Return: follow it
    std::thread::sleep(Duration::from_secs(5));
    let saved =
        node.call("credentials.code", json!({"form": "login", "session": {"id": "live_1"}, "code": "save"})).unwrap();
    let record: serde_json::Value = serde_json::from_str(saved["value"].as_str().unwrap()).unwrap();
    println!("saved URL: {}", record["vars"]["URL"]);
    assert_ne!(record["vars"]["URL"], "https://example.com", "Tab and Return followed the link");
    pane.wait(Duration::from_secs(5), |p| p.closed().is_some()).unwrap();

    // 2. a cookie set in a pane's session reaches an agent's run through STATE
    ask("live_2", "https://httpbin.org/cookies/set?pb_live=yes-1234567");
    std::thread::sleep(Duration::from_secs(5));
    let saved =
        node.call("credentials.code", json!({"form": "login", "session": {"id": "live_2"}, "code": "save"})).unwrap();
    let record = saved["value"].as_str().unwrap().to_string();
    assert!(record.contains("pb_live"), "the session's cookie is in the state");
    drop(node);
    let hidden =
        json!({"kind": "env", "vars": {"URL": "https://httpbin.org/forms/post", "PASSWORD": "live-hidden-123"},
                        "plain": ["URL"]})
        .to_string();
    let node = start(vec![("browser.live".into(), record), ("browser.form".into(), hidden)]);
    let run = |args: serde_json::Value, login: &str| {
        node.run("browser.run", json!({"args": args, "agent": "live-test", "login": login})).unwrap().result
    };
    // later runs without the login read what the signed-in session shows (each run with it signs in again, which
    // reopens the login's URL)
    let bare =
        |args: serde_json::Value| node.run("browser.run", json!({"args": args, "agent": "live-test"})).unwrap().result;
    // Save kept the origin as URL, so the sign-in lands on httpbin's home page, which never lists cookies: the run
    // goes on to /cookies, which does
    let opened = run(json!(["open", "https://httpbin.org/cookies"]), "live");
    println!("signed_in: {}, opened: {}", opened["signed_in"], opened["data"]["url"]);
    assert_eq!(opened["signed_in"], "state");
    assert_eq!(opened["ok"], true, "{opened}");
    let got = bare(json!(["get", "text", "body"]));
    println!("cookies page: {}", got["data"]);
    assert!(got.to_string().contains("pb_live"), "httpbin shows the cookie the pane set: {got}");

    // 3. a hidden value, on stdin, on its own site; scrubbed from what echoes it
    let filled = run(json!(["fill", "input[name=custname]", "{{PASSWORD}}"]), "form");
    println!("fill: {filled}");
    assert_eq!(filled["ok"], true, "{filled}");
    // read back without the login: the session has had the value, so it's scrubbed all the same
    let echo = bare(json!(["get", "value", "input[name=custname]"]));
    println!("get value: {echo}");
    assert!(!echo.to_string().contains("live-hidden-123") && echo.to_string().contains("***"));
    let len = bare(json!(["eval", "document.querySelector('input[name=custname]').value.length"]));
    println!("length: {}", len["data"]);
    assert!(len.to_string().contains("15"), "the value went in whole: {len}");
    run(json!(["close"]), "form");
}
