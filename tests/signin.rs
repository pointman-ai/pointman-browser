//! Sign in yourself (EXT-156) under the stand-in node, with a fake agent-browser and a fake stream: `credentials.sign_in`
//! starts a session of its own and answers with the pane, the pane shows its frames while someone watches and takes
//! their input while they hold Control, Save seals the session as the login, and closing, Save or 15 minutes idle end
//! it and delete its files.

mod common;

use std::time::Duration;

use common::*;
use pointman_extension::testing::StandIn;
use serde_json::{json, Value};

fn sign_in(node: &StandIn, id: &str, start: Option<&str>) -> Result<Value, String> {
    let mut req = json!({"form": "login", "label": "stagesheep", "person": {"name": "james"}, "session": {"id": id},
                         "redirectUri": "https://board.example/oauth/callback"});
    if let Some(s) = start {
        req["start"] = json!(s);
    }
    node.call("credentials.sign_in", req).map_err(|e| format!("{e:#}"))
}

fn save(node: &StandIn, id: &str, code: &str) -> Result<Value, String> {
    node.call("credentials.code", json!({"form": "login", "session": {"id": id}, "code": code}))
        .map_err(|e| format!("{e:#}"))
}

fn signin_dir(node: &StandIn, id: &str) -> std::path::PathBuf {
    node.state_dir_path().join("signins").join(format!("signin-{id}"))
}

fn sessions_of(fake: &Fake, word: &str) -> Vec<String> {
    fake.calls_of(word).iter().filter_map(|c| c.flag("--session")).collect()
}

#[test]
fn a_sign_in_starts_its_own_session_and_answers_with_the_pane() {
    let fake = Fake::new();
    let _stream = FakeStream::start(&fake);
    let node = node(&fake, json!({}));
    let answer = sign_in(&node, "sin_7", Some("https://stage.example.dev/login")).unwrap();
    assert_eq!(answer["pane"], json!({"open": "signin", "target": {"session": "sin_7"}}));
    assert_eq!(answer["prompt"], "Sign in to stage.example.dev in the browser pane, then tap Save.");
    assert!(answer.get("url").is_none(), "the pane, in place of a link");
    let open = fake.calls_of("open");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].flag("--session").as_deref(), Some("signin-sin_7"), "a session of its own, never an agent's");
    assert_eq!(open[0].args.last().map(String::as_str), Some("https://stage.example.dev/login"));
    assert!(signin_dir(&node, "sin_7").is_dir());
    // asked again: the same pane, not a second session
    sign_in(&node, "sin_7", Some("https://stage.example.dev/login")).unwrap();
    assert_eq!(fake.calls_of("open").len(), 1);
}

#[test]
fn a_sign_in_needs_an_https_start_and_the_login_form() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    for (start, why) in [
        (None, "https link"),
        (Some("http://stage.example.dev/"), "https link"),
        (Some("file:///etc/passwd"), "https link"),
        (Some("javascript:alert(1)"), "https link"),
    ] {
        let e = sign_in(&node, "sin_1", start).unwrap_err();
        assert!(e.contains(why), "{start:?}: {e}");
    }
    let e = node
        .call(
            "credentials.sign_in",
            json!({"form": "other", "label": "x", "session": {"id": "s"}, "start": "https://a.example/"}),
        )
        .unwrap_err();
    assert!(format!("{e:#}").contains("one sign-in form, login"));
    assert!(fake.calls().is_empty(), "nothing started");
}

#[test]
fn the_pane_shows_frames_while_watched_and_takes_input_under_control() {
    let fake = Fake::new();
    let stream = FakeStream::start(&fake);
    let node = node(&fake, json!({}));
    sign_in(&node, "sin_2", Some("https://stage.example.dev/login")).unwrap();
    let pane = node.open_pane("signin", json!({"session": "sin_2"})).unwrap();
    let video = pane.video().unwrap();
    assert_eq!((video.width, video.height), (1280, 720));
    assert_eq!(pane.title().as_deref(), Some("Sign in to stage.example.dev"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stream.connections.load(std::sync::atomic::Ordering::SeqCst), 0, "no stream while nobody watches");

    pane.viewers(1).unwrap();
    pane.wait(Duration::from_secs(5), |p| p.frame_count() > 0).unwrap();
    let f = &pane.video_frames()[0];
    assert_eq!((f.width, f.height), (64, 48), "the frame's own size, from its JPEG");
    assert_eq!(f.jpeg, tiny_jpeg());

    // no grant: refused, and nothing reaches the page
    let click = json!({"kind": "pointer", "x": 0.5, "y": 0.25, "phase": "down", "button": "left", "clicks": 1});
    let e = pane.control_input(click.clone()).unwrap_err();
    assert!(format!("{e:#}").contains("control is off"), "{e:#}");
    let until = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    pane.control(Some(until), "james", "MacBook Pro").unwrap();
    pane.control_input(click).unwrap();
    pane.control_input(json!({"kind": "key", "code": 0, "chars": "a"})).unwrap();
    pane.control_input(json!({"kind": "text", "text": "hi"})).unwrap();
    let got = stream.wait_inputs(5);
    assert_eq!(
        got[0],
        json!({"type": "input_mouse", "eventType": "mousePressed", "x": 640.0, "y": 180.0, "button": "left", "clickCount": 1, "modifiers": 0}),
        "0-1 of the picture, on the page's 1280x720"
    );
    assert_eq!((got[1]["eventType"].as_str(), got[1]["text"].as_str()), (Some("keyDown"), Some("a")));
    assert_eq!(got[2]["eventType"], "keyUp");
    assert_eq!((got[3]["eventType"].as_str(), got[4]["text"].as_str()), (Some("char"), Some("i")));

    // control ends: the button still down is let go
    pane.control(None, "", "").unwrap();
    let got = stream.wait_inputs(6);
    assert_eq!((got[5]["eventType"].as_str(), got[5]["button"].as_str()), (Some("mouseReleased"), Some("left")));
    assert!(pane.control_input(json!({"kind": "text", "text": "x"})).is_err());
}

#[test]
fn someone_elses_sign_in_doesnt_open() {
    let fake = Fake::new();
    let _stream = FakeStream::start(&fake);
    let node = node(&fake, json!({}));
    node.call(
        "credentials.sign_in",
        json!({"form": "login", "label": "x", "person": {"name": "alex"}, "session": {"id": "sin_3"}, "start": "https://a.example/"}),
    )
    .unwrap();
    let e = node.open_pane("signin", json!({"session": "sin_3"})).err().expect("refused");
    assert!(format!("{e:#}").contains("someone else's"), "{e:#}");
    let e = node.open_pane("signin", json!({"session": "nope"})).err().expect("refused");
    assert!(format!("{e:#}").contains("no such sign-in"), "{e:#}");
}

#[test]
fn save_seals_the_session_as_the_login_and_ends_it() {
    let fake = Fake::new();
    let _stream = FakeStream::start(&fake);
    let node = node(&fake, json!({}));
    sign_in(&node, "sin_4", Some("https://stage.example.dev/login")).unwrap();
    let pane = node.open_pane("signin", json!({"session": "sin_4"})).unwrap();
    let e = save(&node, "sin_4", "123456").unwrap_err();
    assert!(e.contains("Save"), "only Save: {e}");
    std::fs::write(fake.dir.path().join("page.url"), "https://stage.example.dev/home?tab=1").unwrap();
    let got = save(&node, "sin_4", "save").unwrap();
    let record: Value = serde_json::from_str(got["value"].as_str().unwrap()).unwrap();
    assert_eq!(record["kind"], "env");
    assert_eq!(record["plain"], json!(["URL"]));
    assert_eq!(record["vars"]["URL"], "https://stage.example.dev", "the origin at save");
    let state: Value = serde_json::from_str(record["vars"]["STATE"].as_str().unwrap()).unwrap();
    assert_eq!(state["cookies"][0]["value"], "cookie-v4lue-1");
    assert!(got.get("note").is_none());
    // ended: the session closed, its folder gone, the pane closed
    assert_eq!(sessions_of(&fake, "close"), ["signin-sin_4"]);
    assert!(!signin_dir(&node, "sin_4").exists(), "its files are deleted");
    pane.wait(Duration::from_secs(5), |p| p.closed().is_some()).unwrap();
    let e = save(&node, "sin_4", "save").unwrap_err();
    assert!(e.contains("has ended"), "{e}");
    // the state file it saved isn't left anywhere the extension writes
    let mut files = Vec::new();
    all_files(node.state_dir_path(), &mut files);
    all_files(node.work_dir(), &mut files);
    for f in files {
        let text = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).to_string();
        assert!(!text.contains("cookie-v4lue-1"), "{} holds the session", f.display());
    }
}

#[test]
fn save_on_a_page_that_isnt_https_keeps_the_sign_in_open() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    sign_in(&node, "sin_5", Some("https://stage.example.dev/login")).unwrap();
    std::fs::write(fake.dir.path().join("page.url"), "about:blank").unwrap();
    let e = save(&node, "sin_5", "save").unwrap_err();
    assert!(e.contains("isn't on an https site"), "{e}");
    assert!(signin_dir(&node, "sin_5").is_dir(), "still open, to sign in and Save again");
}

#[test]
fn closing_the_pane_ends_the_sign_in_and_deletes_its_files() {
    let fake = Fake::new();
    let _stream = FakeStream::start(&fake);
    let node = node(&fake, json!({}));
    sign_in(&node, "sin_6", Some("https://stage.example.dev/login")).unwrap();
    let pane = node.open_pane("signin", json!({"session": "sin_6"})).unwrap();
    pane.close("closed").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while signin_dir(&node, "sin_6").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!signin_dir(&node, "sin_6").exists());
    assert_eq!(sessions_of(&fake, "close"), ["signin-sin_6"]);
    assert!(save(&node, "sin_6", "save").unwrap_err().contains("has ended"));
}

#[test]
fn a_sign_in_left_idle_ends_by_itself() {
    let fake = Fake::new();
    let home = tempfile::tempdir().unwrap();
    let node = StandIn::new(ROOT)
        .env("HOME", home.path().to_str().unwrap())
        .roots([fake.dir.path().to_path_buf()])
        .binary(env!("CARGO_BIN_EXE_browser"))
        .machine("testbox")
        .env("POINTMAN_BROWSER_CLI", fake.path())
        .env("POINTMAN_BROWSER_SIGNIN_IDLE_SECS", "1")
        .env("POINTMAN_BROWSER_SWEEP_SECS", "1")
        .start()
        .unwrap();
    sign_in(&node, "sin_8", Some("https://stage.example.dev/login")).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while signin_dir(&node, "sin_8").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!signin_dir(&node, "sin_8").exists(), "ended after its idle time");
    assert_eq!(sessions_of(&fake, "close"), ["signin-sin_8"]);
}

fn all_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}
