//! NixOS: with no `chrome` setting the extension builds Chromium with `nix` into its state folder and uses that.
//! `nix` here is a stand-in script that logs its words and makes `<out-link>/bin/chromium`; nothing reaches the
//! network or a store.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::*;
use pointman_extension::testing::StandIn;
use serde_json::{json, Value};

/// A folder with a `nix` that logs each call's words to `nix.log` and makes the out-link's `bin/chromium`, or, when
/// `fail`, prints a reason and exits 1.
fn stand_in_nix(fail: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let body = if fail {
        "echo 'error: unable to download cache.nixos.org' >&2\nexit 1\n".to_string()
    } else {
        "prev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--out-link\" ]; then mkdir -p \"$a/bin\"; : > \"$a/bin/chromium\"; chmod +x \"$a/bin/chromium\"; fi\n  prev=\"$a\"\ndone\n".to_string()
    };
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done >> \"{d}/nix.log\"\necho ---- >> \"{d}/nix.log\"\n{body}",
        d = dir.path().display()
    );
    let p = dir.path().join("nix");
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn nix_calls(dir: &Path) -> Vec<Vec<String>> {
    let log = std::fs::read_to_string(dir.join("nix.log")).unwrap_or_default();
    log.split("----\n").filter(|c| !c.trim().is_empty()).map(|c| c.lines().map(String::from).collect()).collect()
}

fn nixos(fake: &Fake, nix: &Path, settings: Value) -> StandIn {
    let home = tempfile::tempdir().unwrap();
    let node = StandIn::new(ROOT)
        .env("HOME", home.path().to_str().unwrap())
        .roots([fake.dir.path().to_path_buf()])
        .binary(env!("CARGO_BIN_EXE_browser"))
        .settings(settings)
        .machine("testbox")
        .env("POINTMAN_BROWSER_CLI", fake.path())
        .env("POINTMAN_BROWSER_NIXOS", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", nix.display()))
        .start()
        .unwrap();
    std::mem::forget(home);
    node
}

fn open(node: &StandIn) -> Result<(), String> {
    node.run("browser.run", json!({"args": ["open", "https://example.com"], "agent": "patchbay"}))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
fn nixos_builds_its_own_chromium_once_and_uses_it() {
    let fake = Fake::new();
    let nix = stand_in_nix(false);
    let node = nixos(&fake, nix.path(), json!({}));
    open(&node).unwrap();

    let calls = nix_calls(nix.path());
    assert_eq!(calls.len(), 1, "{calls:?}: built at start or at the first call, once");
    let link: PathBuf = node.state_dir_path().join("chromium");
    let rev = "f4a03c008967841f2a0b3cb7c436fc6fc31671f6";
    assert_eq!(
        calls[0],
        [
            "build",
            &format!("github:NixOS/nixpkgs/{rev}#chromium"),
            "--out-link",
            link.to_str().unwrap(),
            "--extra-experimental-features",
            "nix-command flakes"
        ]
    );
    let used = &fake.calls_of("open")[0].chrome;
    assert_eq!(Path::new(used), link.join("bin/chromium"), "agent-browser is given the out-link's browser");

    open(&node).unwrap();
    assert_eq!(nix_calls(nix.path()).len(), 1, "the out-link is there: nix isn't run again");
    let chrome = part(&node, "chrome");
    assert_eq!(chrome["state"], "STATE_OK", "{chrome}");
    assert!(chrome["why"].as_str().unwrap_or("").contains("chromium"), "{chrome}");
}

#[test]
fn the_chrome_setting_wins_and_nix_is_not_run() {
    let fake = Fake::new();
    let nix = stand_in_nix(false);
    let mine = fake.dir.path().join("my-chromium");
    std::fs::write(&mine, "").unwrap();
    let node = nixos(&fake, nix.path(), json!({"chrome": mine.to_str().unwrap()}));
    open(&node).unwrap();
    assert!(nix_calls(nix.path()).is_empty());
    assert_eq!(Path::new(&fake.calls_of("open")[0].chrome), mine);
}

#[test]
fn a_failed_build_says_nix_s_reason_and_retries_without_telling_anyone_to_install() {
    let fake = Fake::new();
    let nix = stand_in_nix(true);
    let node = nixos(&fake, nix.path(), json!({}));
    let err = open(&node).unwrap_err();
    assert!(err.contains("nix couldn't build Chromium"), "{err}");
    assert!(err.contains("unable to download cache.nixos.org"), "{err}");
    let chrome = part(&node, "chrome");
    assert_eq!(chrome["state"], "STATE_FAILING", "{chrome}");
    let text = chrome["why"].as_str().unwrap_or("").to_string();
    assert!(text.contains("unable to download cache.nixos.org") && text.contains("retries on the next call"), "{text}");
    for word in ["programs.chromium", "pkgs.chromium", "set this extension"] {
        assert!(!text.contains(word), "{text}");
    }
    let before = nix_calls(nix.path()).len();
    let _ = open(&node);
    assert_eq!(nix_calls(nix.path()).len(), before + 1, "the next call tries again");
}
