//! A test login the agent never sees: the extension reads the vault item, sends the Access headers to one origin,
//! types the password on stdin, and nothing of it comes back in an answer, a job's params or a run's files.

mod common;

use common::*;
use serde_json::json;

const PASTE: &str = "URL=https://stage.example.dev/sign-in\nACCESS_CLIENT_ID=abc.access\nACCESS_CLIENT_SECRET=s3cr3t-token\nEMAIL=bot@example.com\nPASSWORD=hunter2 pass\n";
const SECRETS: [&str; 4] = ["s3cr3t-token", "bot@example.com", "hunter2", "abc.access.s"];

fn leaks(text: &str) -> Vec<&'static str> {
    ["s3cr3t-token", "bot@example.com", "hunter2"].into_iter().filter(|s| text.contains(s)).collect()
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

#[test]
fn a_login_signs_the_session_in_and_nothing_comes_back() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", PASTE)]);
    let done = node
        .run(
            "browser.run",
            json!({"args": ["open", "https://stage.example.dev/"], "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap();
    assert_eq!(done.result["ok"], true);
    assert_eq!(done.result["login"], "stagesheep");

    let words: Vec<_> = fake.calls().iter().map(|c| c.args.join(" ")).collect();
    assert!(words[0].contains("--config"), "the Access headers ride in a config file: {words:?}");
    assert!(words[0].contains("open https://stage.example.dev/sign-in"), "{words:?}");
    assert!(!words.iter().any(|w| w.contains("--headers")), "no headers on any command line: {words:?}");
    for w in &words {
        assert!(!w.contains("s3cr3t-token") && !w.contains("abc.access"), "the Access token is in no argv: {w}");
    }
    let calls = std::fs::read_to_string(fake.dir.path().join("calls.log")).unwrap();
    assert!(!calls.contains("s3cr3t-token"), "nothing of the token in what the stand-in saw as argv or env");
    let dir = fake.dir.path();
    let seen = std::fs::read_to_string(dir.join("config.seen")).unwrap();
    assert!(seen.contains("CF-Access-Client-Secret"), "the headers ride in the config file");
    assert!(seen.contains("s3cr3t-token"), "{seen}");
    assert_eq!(std::fs::read_to_string(dir.join("config.mode")).unwrap().trim(), "600");
    let path = std::fs::read_to_string(dir.join("config.path")).unwrap();
    assert!(!std::path::Path::new(path.trim()).exists(), "the config file is deleted right after the open");
    assert!(words[1].contains("auth save pb-patchbay --url https://stage.example.dev/sign-in"), "{words:?}");
    assert!(words[1].contains("--password-stdin"));
    assert!(!words[1].contains("hunter2"), "the password is on stdin, never in argv: {words:?}");
    assert_eq!(std::fs::read_to_string(fake.dir.path().join("stdin.log")).unwrap(), "hunter2 pass");
    assert!(words[2].starts_with("--session patchbay --json auth login pb-patchbay"), "{words:?}");
    assert_eq!(std::fs::read_to_string(fake.dir.path().join("deleted.log")).unwrap().trim(), "deleted pb-patchbay");
    assert!(
        words.last().unwrap().contains("open https://stage.example.dev/"),
        "then the agent's own command: {words:?}"
    );
    assert_eq!(fake.calls().last().unwrap().args.iter().filter(|a| a.contains("--headers")).count(), 0);

    // nothing of the record in the answer, though the fake's `auth login` echoes it
    let answer = done.result.to_string();
    assert!(leaks(&answer).is_empty(), "{answer}");
}

#[test]
fn no_value_reaches_params_answers_errors_or_files() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", PASTE)]);
    // a failed sign-in's error carries none of it, though the CLI's own message did
    std::fs::write(fake.dir.path().join("login-fails"), "").unwrap();
    let e =
        node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "stagesheep"})).unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("signing in failed"), "{e}");
    assert!(leaks(&e).is_empty(), "{e}");
    assert_eq!(
        std::fs::read_to_string(fake.dir.path().join("deleted.log")).unwrap().lines().count(),
        1,
        "the saved profile is deleted even when the sign-in fails"
    );
    std::fs::remove_file(fake.dir.path().join("login-fails")).unwrap();

    let ok =
        node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "stagesheep"})).unwrap();
    let mut files = Vec::new();
    all_files(node.work_dir(), &mut files);
    all_files(node.state_dir_path(), &mut files);
    for f in files {
        let text = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).to_string();
        assert!(leaks(&text).is_empty(), "{} holds a value", f.display());
    }
    assert!(leaks(&ok.result.to_string()).is_empty());
    let _ = SECRETS;
}

#[test]
fn a_run_with_a_login_holds_its_session_and_the_lane() {
    let fake = Fake::new();
    let home = tempfile::tempdir().unwrap();
    let lane = fake_lane(home.path());
    let node = node_with_secrets(&fake, json!({"lock": lane.to_str().unwrap()}), &[("browser.stagesheep", PASTE)]);
    let start = node
        .run(
            "browser.test_start",
            json!({"name": "Stagesheep", "checklist": "- Open the page\n", "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap();
    assert_eq!(start.result["ok"], true);
    assert!(leaks(&start.result.to_string()).is_empty());
    assert!(fake.calls_of("auth").len() >= 2, "the session is signed in at the start");
    let log = lane_log(home.path());
    assert!(log.iter().any(|l| l.starts_with("take patchbay")), "{log:?}");
}

#[test]
fn a_missing_or_bad_login_stops_before_anything_is_typed() {
    let fake = Fake::new();
    let bad = PASTE.replace("EMAIL=bot@example.com\n", "");
    let node = node_with_secrets(&fake, json!({}), &[("browser.broken", &bad)]);
    for (label, why) in [("nope", "no login named nope"), ("broken", "isn't usable"), ("../x", "label")] {
        let e =
            node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": label})).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains(why), "{e}");
        assert!(leaks(&e).is_empty());
    }
    assert!(fake.calls_of("auth").is_empty() && fake.calls_of("open").is_empty(), "nothing reached the browser");
}

#[test]
fn an_env_record_in_the_vault_works_too() {
    let fake = Fake::new();
    let rec =
        json!({"kind": "env", "vars": {"URL": "https://stage.example.dev/sign-in", "ACCESS_CLIENT_ID": "abc.access",
        "ACCESS_CLIENT_SECRET": "s3cr3t-token", "EMAIL": "bot@example.com", "PASSWORD": "hunter2 pass"}})
        .to_string();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &rec)]);
    let done =
        node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "stagesheep"})).unwrap();
    assert_eq!(done.result["ok"], true);
    assert!(leaks(&done.result.to_string()).is_empty());
}
