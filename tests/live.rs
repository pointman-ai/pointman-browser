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
