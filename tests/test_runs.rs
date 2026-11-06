//! A checklist run against the stand-in node and the fake CLI: start, steps with stills, finish with a report; the
//! heavy-job lane held from start to finish; a still from a file; a failed step shown first.

mod common;

use common::*;
use serde_json::{json, Value};

const CHECKLIST: &str =
    "- Open the home page\n  expect: the title says Example Domain\n- Click Learn more\n- Never reached\n";

fn start(node: &pointman_extension::testing::StandIn) -> String {
    let done = node
        .run("browser.test.start", json!({"name": "Staging: home", "checklist": CHECKLIST, "agent": "@patchbay"}))
        .unwrap();
    assert_eq!(done.result["ok"], true);
    done.result["run"].as_str().unwrap().to_string()
}

fn step(node: &pointman_extension::testing::StandIn, run: &str, n: u64, ok: bool, extra: Value) -> Value {
    let mut p = json!({"run": run, "step": n, "ok": ok});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        p[k] = v;
    }
    node.run("browser.test.step", p).unwrap().result
}

fn report_text(done: &pointman_extension::testing::Done) -> String {
    let out = done.outputs.iter().find(|o| o.kind == "document").expect("report.html is an output");
    assert!(out.path.ends_with("report.html"), "{:?}", out.path);
    std::fs::read_to_string(&out.path).unwrap()
}

#[test]
fn a_run_ends_in_a_report_with_a_still_for_each_step() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let run = start(&node);
    assert!(run.starts_with("run-"), "{run}");

    let one =
        node.run("browser.test.step", json!({"run": run, "step": 1, "ok": true, "note": "title matched"})).unwrap();
    assert_eq!(one.result["shot"], true);
    assert_eq!(one.outputs.len(), 1);
    assert_eq!(one.outputs[0].kind, "image", "a still comes back as a PNG output");
    let shots = fake.calls_of("screenshot");
    assert_eq!(shots[0].flag("--session").as_deref(), Some("patchbay"), "the still is of the agent's own session");
    let two = step(&node, &run, 2, false, json!({"note": "no link called Learn more"}));
    assert_eq!(two["recorded_so_far"], json!({"passed": 1, "failed": 1, "left": 1}));

    let done = node.run("browser.test.finish", json!({"run": run})).unwrap();
    let r = &done.result;
    assert_eq!(
        (r["passed"].as_u64(), r["failed"].as_u64(), r["skipped"].as_u64(), r["steps"].as_u64()),
        (Some(1), Some(1), Some(1), Some(3))
    );
    let html = report_text(&done);
    assert!(html.contains("Staging: home: 1 of 3 steps failed"), "headline");
    assert!(html.contains("Run by @patchbay on testbox against https://staging.example/"), "lead names the url");
    assert!(html.contains("First failure: step 2, Click Learn more: no link called Learn more."));
    assert_eq!(html.matches("data:image/png;base64,").count(), 2, "one still for each recorded step, once");
    assert!(html.contains("Step 3 was not run: Never reached"));
    assert!(!html.contains("{{"));
}

#[test]
fn a_step_can_bring_its_own_still() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let run = start(&node);
    let still = fake.dir.path().join("mac-app.png");
    std::fs::write(&still, b"\x89PNG\r\n\x1a\nMACAPP").unwrap();
    let r = step(&node, &run, 1, true, json!({"file": still.to_str().unwrap()}));
    assert_eq!(r["shot"], true);
    assert!(fake.calls_of("screenshot").is_empty(), "the browser isn't asked for one");

    let not_image = fake.dir.path().join("notes.png");
    std::fs::write(&not_image, b"<html>").unwrap();
    let e = node
        .run("browser.test.step", json!({"run": run, "step": 2, "ok": true, "file": not_image.to_str().unwrap()}))
        .unwrap_err();
    assert!(e.to_string().contains("not a PNG or JPEG"), "{e}");
    let e = node
        .run("browser.test.step", json!({"run": run, "step": 2, "ok": true, "file": "/nonexistent/x.png"}))
        .unwrap_err();
    assert!(e.to_string().contains("not there"), "{e}");

    let done = node.run("browser.test.finish", json!({"run": run})).unwrap();
    assert!(
        report_text(&done).contains("data:image/png;base64,iVBORw0KGgpNQUNBUFA="),
        "the file's bytes are the still"
    );
}

#[test]
fn no_browser_means_a_step_without_a_still_not_a_failed_job() {
    let fake = Fake::new();
    std::fs::write(fake.dir.path().join("no-browser"), "").unwrap();
    let node = node(&fake, json!({}));
    let run = start(&node);
    let r = step(&node, &run, 1, true, json!({}));
    assert_eq!(r["shot"], false);
    assert!(r["shot_error"].as_str().unwrap().contains("No page open"), "{r}");
    let r = step(&node, &run, 2, true, json!({"shot": false}));
    assert_eq!(r["shot_error"], Value::Null, "no still asked for, none missed");
    let done = node.run("browser.test.finish", json!({"run": run})).unwrap();
    assert!(report_text(&done).contains("2 recorded steps have no still"));
}

#[test]
fn bad_calls_are_refused_with_what_to_do() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let run = start(&node);
    let err = |params: Value, kind: &str| node.run(kind, params).unwrap_err().to_string();
    assert!(err(json!({"name": "x", "checklist": "no steps", "agent": "a"}), "browser.test.start").contains("no steps"));
    assert!(err(json!({"name": "x", "checklist": "- a", "agent": ""}), "browser.test.start")
        .contains("agent is your handle"));
    assert!(err(json!({"run": run, "step": 9, "ok": true}), "browser.test.step").contains("steps 1 to 3"));
    assert!(err(json!({"run": run, "step": 1}), "browser.test.step").contains("ok is true or false"));
    assert!(err(json!({"run": "../../etc", "step": 1, "ok": true}), "browser.test.step").contains("run is the id"));
    assert!(err(json!({"run": "run-1-1", "step": 1, "ok": true}), "browser.test.step").contains("no run run-1-1"));
    node.run("browser.test.finish", json!({"run": run})).unwrap();
    assert!(err(json!({"run": run, "step": 1, "ok": true}), "browser.test.step").contains("is finished"));
}

#[test]
fn a_run_holds_the_heavy_job_lane_from_start_to_finish() {
    let fake = Fake::new();
    let lane_dir = tempfile::tempdir().unwrap();
    let script = fake_lane(lane_dir.path());
    let node = node(&fake, json!({"lock": script.to_str().unwrap()}));
    let run = start(&node);
    let log = lane_log(lane_dir.path());
    assert_eq!(log.len(), 2, "take waited once (exit 3), then took it: {log:?}");
    assert!(log.iter().all(|l| l.starts_with("take patchbay UI test: Staging: home")), "{log:?}");
    step(&node, &run, 1, true, json!({}));
    assert_eq!(lane_log(lane_dir.path()).len(), 2, "steps keep it");
    node.run("browser.test.finish", json!({"run": run})).unwrap();
    assert_eq!(lane_log(lane_dir.path()).last().unwrap(), "release patchbay");

    // a run left open gives up its lane when the same agent starts another
    let again = start(&node);
    let again2 = start(&node);
    assert_ne!(again, again2);
    let log = lane_log(lane_dir.path());
    assert_eq!(log.iter().filter(|l| l.starts_with("release")).count(), 2, "{log:?}");
    let e = node.run("browser.test.step", json!({"run": again, "step": 1, "ok": true, "shot": false})).unwrap_err();
    assert!(e.to_string().contains("is finished"), "{e}");
}

#[test]
fn no_lock_setting_no_lane() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    let run = start(&node);
    let done = node.run("browser.test.finish", json!({"run": run})).unwrap();
    assert_eq!(done.result["failed"], 0);
}

#[test]
fn a_lane_script_without_the_lock_setting_refuses_the_run() {
    let fake = Fake::new();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("lane")).unwrap();
    std::fs::write(home.path().join("lane/heavy.sh"), "exit 0\n").unwrap();
    let node = node_in(&fake, json!({}), home.path());
    let err = node
        .run("browser.test.start", json!({"name": "x", "checklist": "- one\n", "agent": "patchbay"}))
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(err.contains("`lock` setting is empty"), "{err}");
}
