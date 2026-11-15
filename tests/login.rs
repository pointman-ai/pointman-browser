//! A test login the agent never sees: the extension reads the vault item (`vars` + `plain`), signs the session in
//! (STATE, then URL + EMAIL/USERNAME + PASSWORD, then URL alone), sends the Access headers to one origin, puts
//! `{{NAME}}`s in, and nothing hidden comes back in an answer, a job's params or a run's files.

mod common;

use common::*;
use serde_json::json;

const SECRETS: [&str; 2] = ["s3cr3t-token", "hunter2"];

fn record(access: bool) -> String {
    let mut vars =
        json!({"URL": "https://stage.example.dev/sign-in", "EMAIL": "bot@example.com", "PASSWORD": "hunter2 pass"});
    if access {
        vars["ACCESS_CLIENT_ID"] = "abc.access".into();
        vars["ACCESS_CLIENT_SECRET"] = "s3cr3t-token".into();
    }
    json!({"kind": "env", "vars": vars, "plain": ["ACCESS_CLIENT_ID", "EMAIL", "URL"]}).to_string()
}

/// A record of the person's own values.
fn own(vars: serde_json::Value, plain: &[&str]) -> String {
    json!({"kind": "env", "vars": vars, "plain": plain}).to_string()
}

/// The hidden values: the email and client id are plain, shown as typed, and the stand-in echoes them.
fn leaks(text: &str) -> Vec<&'static str> {
    SECRETS.into_iter().filter(|s| text.contains(s)).collect()
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
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &record(true))]);
    let done = node
        .run(
            "browser.run",
            json!({"args": ["open", "https://stage.example.dev/"], "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap();
    assert_eq!(done.result["ok"], true);
    assert_eq!(done.result["login"], "stagesheep");
    assert_eq!(done.result["signed_in"], "password");
    let names: Vec<_> = done.result["vars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["name"].as_str().unwrap(), v["hidden"] == true))
        .collect();
    assert_eq!(
        names,
        [
            ("ACCESS_CLIENT_ID", false),
            ("ACCESS_CLIENT_SECRET", true),
            ("EMAIL", false),
            ("PASSWORD", true),
            ("URL", false)
        ],
        "the answer lists the names, hidden ones marked"
    );

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
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &record(true))]);
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
}

#[test]
fn a_run_with_a_login_holds_its_session_and_the_lane() {
    let fake = Fake::new();
    let home = tempfile::tempdir().unwrap();
    let lane = fake_lane(home.path());
    let node =
        node_with_secrets(&fake, json!({"lock": lane.to_str().unwrap()}), &[("browser.stagesheep", &record(true))]);
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
    let bad = record(true).replace("https://stage.example.dev/sign-in", "http://stage.example.dev/sign-in");
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
fn a_login_without_access_signs_in_with_no_config_and_no_headers() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.plain", &record(false))]);
    let done = node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "plain"})).unwrap();
    assert_eq!(done.result["ok"], true);
    let words: Vec<_> = fake.calls().iter().map(|c| c.args.join(" ")).collect();
    assert!(words[0].contains("open https://stage.example.dev/sign-in"), "{words:?}");
    assert!(!words.iter().any(|w| w.contains("--config") || w.contains("--headers")), "{words:?}");
    assert!(!fake.dir.path().join("config.seen").exists(), "no config file was made");
    assert_eq!(std::fs::read_to_string(fake.dir.path().join("stdin.log")).unwrap(), "hunter2 pass");
    assert!(leaks(&done.result.to_string()).is_empty());
}

#[test]
fn a_password_with_a_trailing_space_reaches_stdin_unchanged() {
    let fake = Fake::new();
    let mut vars = serde_json::from_str::<serde_json::Value>(&record(false)).unwrap();
    vars["vars"]["PASSWORD"] = " hunter2 pass ".into();
    let node = node_with_secrets(&fake, json!({}), &[("browser.plain", &vars.to_string())]);
    let done = node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "plain"})).unwrap();
    assert_eq!(done.result["ok"], true);
    assert_eq!(std::fs::read_to_string(fake.dir.path().join("stdin.log")).unwrap(), " hunter2 pass ");
    assert!(leaks(&done.result.to_string()).is_empty());
}

#[test]
fn one_access_field_alone_is_refused_before_anything_is_typed() {
    let fake = Fake::new();
    let mut vars = serde_json::from_str::<serde_json::Value>(&record(true)).unwrap();
    vars["vars"].as_object_mut().unwrap().remove("ACCESS_CLIENT_ID");
    let node = node_with_secrets(&fake, json!({}), &[("browser.alone", &vars.to_string())]);
    let e = node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "alone"})).unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("isn't usable") && e.contains("ACCESS_CLIENT_ID"), "{e}");
    assert!(leaks(&e).is_empty(), "{e}");
    assert!(fake.calls_of("auth").is_empty() && fake.calls_of("open").is_empty());
}

#[test]
fn a_saved_session_is_loaded_from_a_private_file_then_the_site_opened() {
    let fake = Fake::new();
    let state =
        json!({"cookies": [{"name": "sid", "value": "cookie-v4lue-1", "domain": "stage.example.dev"}], "origins": []});
    let rec = own(
        json!({"URL": "https://stage.example.dev", "STATE": state.to_string(), "PASSWORD": "hunter2 pass",
                         "EMAIL": "bot@example.com"}),
        &["EMAIL", "URL"],
    );
    let node = node_with_secrets(&fake, json!({}), &[("browser.saved", &rec)]);
    let done = node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "saved"})).unwrap();
    assert_eq!(done.result["signed_in"], "state", "STATE comes first, before a password");
    let words: Vec<_> = fake.calls().iter().map(|c| c.args.join(" ")).collect();
    assert!(words[0].starts_with("--session patchbay --json state load "), "{words:?}");
    assert!(words[1].ends_with("open https://stage.example.dev"), "{words:?}");
    assert!(fake.calls_of("auth").is_empty(), "no password sign-in: {words:?}");
    let dir = fake.dir.path();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(dir.join("state.loaded")).unwrap()).unwrap(),
        state
    );
    assert_eq!(std::fs::read_to_string(dir.join("state.mode")).unwrap().trim(), "600");
    let path = std::fs::read_to_string(dir.join("state.path")).unwrap();
    assert!(!std::path::Path::new(path.trim()).exists(), "the state file is deleted straight after");
    let hidden =
        done.result["vars"].as_array().unwrap().iter().find(|v| v["name"] == "STATE").unwrap()["hidden"].clone();
    assert_eq!(hidden, true);
    let answer = done.result.to_string();
    assert!(!answer.contains("cookie-v4lue-1") && leaks(&answer).is_empty(), "{answer}");
    for c in fake.calls() {
        assert!(!c.args.join(" ").contains("cookie-v4lue-1"), "the state is in no argv");
    }
}

#[test]
fn a_url_alone_is_opened_and_nothing_else() {
    let fake = Fake::new();
    let node = node_with_secrets(
        &fake,
        json!({}),
        &[("browser.docs", &own(json!({"URL": "https://docs.example.dev/start"}), &["URL"]))],
    );
    let done = node.run("browser.run", json!({"args": ["snapshot"], "agent": "patchbay", "login": "docs"})).unwrap();
    assert_eq!(done.result["signed_in"], "open");
    let words: Vec<String> = fake.calls().iter().map(|c| c.word()).collect();
    assert_eq!(words, ["open", "snapshot"]);
}

#[test]
fn a_hidden_value_goes_in_on_stdin_and_only_on_the_logins_site() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &record(false))]);
    std::fs::write(fake.dir.path().join("page.url"), "https://stage.example.dev/sign-in?next=/").unwrap();
    let done = node
        .run(
            "browser.run",
            json!({"args": ["fill", "@e2", "{{PASSWORD}}"], "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap();
    assert_eq!(done.result["ok"], true, "{}", done.result);
    let stdin = std::fs::read_to_string(fake.dir.path().join("batch.stdin")).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&stdin).unwrap(), json!([["fill", "@e2", "hunter2 pass"]]));
    let batch = fake.calls_of("batch");
    assert_eq!(batch.len(), 1);
    assert!(batch[0].args.contains(&"--omit-command".to_string()));
    for c in fake.calls() {
        assert!(!c.args.join(" ").contains("hunter2"), "the password is in no argv: {:?}", c.args);
    }
    // the fake echoes what it typed: scrubbed
    let answer = done.result.to_string();
    assert!(leaks(&answer).is_empty() && answer.contains("***"), "{answer}");

    // another site: refused, and nothing typed
    std::fs::write(fake.dir.path().join("page.url"), "https://evil.example/login").unwrap();
    std::fs::remove_file(fake.dir.path().join("batch.stdin")).unwrap();
    let e = node
        .run(
            "browser.run",
            json!({"args": ["type", "#pw", "{{PASSWORD}}"], "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("only on https://stage.example.dev") && e.contains("https://evil.example"), "{e}");
    assert!(!fake.dir.path().join("batch.stdin").exists(), "nothing was typed");
    assert!(leaks(&e).is_empty());
}

#[test]
fn shown_values_go_anywhere_in_argv() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &record(false))]);
    let done = node
        .run(
            "browser.run",
            json!({"args": ["fill", "#email", "{{EMAIL}}"], "agent": "patchbay", "login": "stagesheep"}),
        )
        .unwrap();
    assert_eq!(done.result["ok"], true);
    let last = fake.calls().last().unwrap().args.join(" ");
    assert!(last.ends_with("fill #email bot@example.com"), "{last}");
    assert!(fake.calls_of("batch").is_empty(), "a shown value needs no stdin");
}

#[test]
fn a_bad_name_stops_the_run_before_the_browser() {
    let fake = Fake::new();
    let node = node_with_secrets(
        &fake,
        json!({}),
        &[("browser.stagesheep", &record(false)), ("browser.nourl", &own(json!({"API_KEY": "k3y-k3y-k3y"}), &[]))],
    );
    for (args, login, why) in [
        (json!(["fill", "@e1", "{{OTP}}"]), Some("stagesheep"), "no OTP"),
        (json!(["open", "https://stage.example.dev/?p={{PASSWORD}}"]), Some("stagesheep"), "{{PASSWORD}} is hidden"),
        (json!(["fill", "{{PASSWORD}}", "x"]), Some("stagesheep"), "never in a URL or a selector"),
        (json!(["fill", "@e1", "{{API_KEY}}"]), Some("nourl"), "no URL"),
        (json!(["fill", "@e1", "{{EMAIL}}"]), None, "pass login"),
    ] {
        let mut params = json!({"args": args, "agent": "patchbay"});
        if let Some(l) = login {
            params["login"] = json!(l);
        }
        let e = format!("{:#}", node.run("browser.run", params).unwrap_err());
        assert!(e.contains(why), "{e} should say {why}");
        assert!(leaks(&e).is_empty() && !e.contains("k3y-k3y"), "{e}");
    }
    assert!(fake.calls().is_empty(), "nothing reached the browser: {:?}", fake.calls());
}

#[test]
fn an_agent_never_gets_a_sign_ins_session() {
    let fake = Fake::new();
    let node = node(&fake, json!({}));
    for agent in ["signin-sin_1", "SignIn-x"] {
        let e = format!("{:#}", node.run("browser.run", json!({"args": ["snapshot"], "agent": agent})).unwrap_err());
        assert!(e.contains("sign-in pane's"), "{e}");
    }
    assert!(fake.calls().is_empty());
}

#[test]
fn a_later_run_without_the_login_is_scrubbed_too() {
    let fake = Fake::new();
    let node = node_with_secrets(&fake, json!({}), &[("browser.stagesheep", &record(false))]);
    std::fs::write(fake.dir.path().join("page.url"), "https://stage.example.dev/sign-in").unwrap();
    node.run(
        "browser.run",
        json!({"args": ["fill", "@e2", "{{PASSWORD}}"], "agent": "patchbay", "login": "stagesheep"}),
    )
    .unwrap();
    // the page still holds what was typed: the fake's `eval echo-typed` gives it back, as `get value` would
    let later = node.run("browser.run", json!({"args": ["eval", "echo-typed"], "agent": "patchbay"})).unwrap();
    let text = later.result.to_string();
    assert!(leaks(&text).is_empty() && text.contains("***"), "{text}");
    // another agent's session never had the login: its answers aren't touched
    let other = node.run("browser.run", json!({"args": ["eval", "echo-typed"], "agent": "deskprobe"})).unwrap();
    assert!(other.result.to_string().contains("hunter2 pass"), "only the session that had it is scrubbed");
}
