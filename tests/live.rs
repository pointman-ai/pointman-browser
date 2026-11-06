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
