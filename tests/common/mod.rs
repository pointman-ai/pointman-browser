//! What the tests share: the extension under the SDK's stand-in node, with a fake `agent-browser` (a shell script
//! that logs each call beside itself and answers as the real one does). No test reaches GitHub or a real browser.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pointman_extension::testing::StandIn;
use serde_json::Value;
use tempfile::TempDir;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The fake CLI. It finds its folder from how it was started, so each test's is a link to this one script, written
/// once: one test writing a script while another starts a process is "text file busy" on Linux.
const FAKE: &str = r#"#!/bin/sh
here=$(dirname "$0")
{
  echo "----"
  echo "cwd=$(pwd)"
  echo "home=${HOME-<none>}"
  echo "sockets=${AGENT_BROWSER_SOCKET_DIR-<none>}"
  echo "chrome=${AGENT_BROWSER_EXECUTABLE_PATH-<none>}"
  echo "leak=${AGENT_BROWSER_SESSION-<none>}"
  for a in "$@"; do echo "arg=$a"; done
} >> "$here/calls.log"
if [ "$1" = "--version" ]; then echo "agent-browser 9.9.9"; exit 0; fi
words=""; prev=""
for a in "$@"; do
  case "$prev" in --session) ;; *) case "$a" in --session|--json) ;; *) words="$words $a";; esac;; esac
  prev="$a"
done
set -- $words
case "$1" in
  open) echo '{"success":true,"data":{"title":"Example Domain","url":"'"$2"'"},"error":null}';;
  snapshot) echo '{"success":true,"data":{"origin":"https://example.com/","refs":{"e1":{"name":"Learn more","role":"link"}},"snapshot":"- link \"Learn more\" [ref=e1]"},"error":null}';;
  big) printf '{"success":true,"data":{"pad":"%s"},"error":null}\n' "$(head -c 700000 /dev/zero | tr '\0' 'x')";;
  click) echo '{"success":false,"data":null,"error":"Element not found: '"$2"'"}'; exit 1;;
  screenshot)
    mkdir -p "$here/shots"
    if [ -f "$here/no-browser" ]; then echo '{"success":false,"data":null,"error":"No page open"}'; exit 1; fi
    if [ -n "$2" ]; then printf '\211PNG\r\n\032\nFILE' > "$2"; echo '{"success":true,"data":{"path":"'"$2"'"},"error":null}'
    else printf '\211PNG\r\n\032\nDATA' > "$here/shots/abc.png"; echo '{"success":true,"data":{"path":"'"$here"'/shots/abc.png"},"error":null}'; fi;;
  get) echo '{"success":true,"data":{"url":"https://staging.example/"},"error":null}';;
  session) echo '{"success":true,"data":{"sessions":["patchbay","deskprobe"]}}';;
  wait) sleep "$2"; echo '{"success":true,"data":{},"error":null}';;
  crash) echo "thread 'main' panicked" >&2; exit 101;;
  *) echo '{"success":false,"data":null,"error":"unknown command"}'; exit 2;;
esac
"#;

fn script() -> PathBuf {
    static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pointman-browser-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-agent-browser");
            std::fs::write(&path, FAKE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        })
        .clone()
}

/// A fake heavy-job lock script: logs `take`/`release` beside itself; `take` exits 3 the first time (its 9-minute
/// wait over), then 0.
pub fn fake_lane(dir: &Path) -> PathBuf {
    let path = dir.join("heavy.sh");
    std::fs::write(
        &path,
        r#"here=$(dirname "$0")
echo "$@" >> "$here/lane.log"
if [ "$1" = take ] && [ ! -f "$here/tried" ]; then touch "$here/tried"; exit 3; fi
exit 0
"#,
    )
    .unwrap();
    path
}

pub fn lane_log(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("lane.log")).unwrap_or_default().lines().map(String::from).collect()
}

/// One test's fake CLI: its folder (the log) and its path.
pub struct Fake {
    pub dir: TempDir,
}

impl Fake {
    pub fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(script(), dir.path().join("agent-browser")).unwrap();
        Fake { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("agent-browser")
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        calls_in(&self.dir.path().join("calls.log"))
    }

    /// The calls whose command (the first word after the scope flags) is `word`.
    pub fn calls_of(&self, word: &str) -> Vec<FakeCall> {
        self.calls().into_iter().filter(|c| c.word() == word).collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeCall {
    pub cwd: String,
    pub home: String,
    pub sockets: String,
    pub chrome: String,
    pub leak: String,
    pub args: Vec<String>,
}

impl FakeCall {
    pub fn word(&self) -> String {
        let mut skip = false;
        for a in &self.args {
            if skip {
                skip = false;
                continue;
            }
            if a == "--session" {
                skip = true;
                continue;
            }
            if a.starts_with("--") {
                continue;
            }
            return a.clone();
        }
        String::new()
    }

    pub fn flag(&self, name: &str) -> Option<String> {
        let at = self.args.iter().position(|a| a == name)?;
        self.args.get(at + 1).cloned()
    }
}

pub fn calls_in(log: &Path) -> Vec<FakeCall> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut calls = Vec::new();
    for record in text.split("----\n").filter(|r| !r.trim().is_empty()) {
        let mut c = FakeCall::default();
        for line in record.lines() {
            match line.split_once('=') {
                Some(("cwd", v)) => c.cwd = v.into(),
                Some(("home", v)) => c.home = v.into(),
                Some(("sockets", v)) => c.sockets = v.into(),
                Some(("chrome", v)) => c.chrome = v.into(),
                Some(("leak", v)) => c.leak = v.into(),
                Some(("arg", v)) => c.args.push(v.into()),
                _ => {}
            }
        }
        calls.push(c);
    }
    calls
}

/// The extension under the stand-in, with the fake as its CLI and `settings`.
pub fn node(fake: &Fake, settings: Value) -> StandIn {
    let home = tempfile::tempdir().unwrap();
    let node = node_in(fake, settings, home.path());
    std::mem::forget(home);
    node
}

/// The same, with `home` as the machine person's home (where `~/lane/heavy.sh` would be).
pub fn node_in(fake: &Fake, settings: Value, home: &std::path::Path) -> StandIn {
    StandIn::new(ROOT)
        .env("HOME", home.to_str().unwrap())
        .binary(env!("CARGO_BIN_EXE_browser"))
        .settings(settings)
        .machine("testbox")
        .env("POINTMAN_BROWSER_CLI", fake.path())
        .env("AGENT_BROWSER_SESSION", "someone-elses")
        .start()
        .unwrap()
}

/// health.check's part `id`.
pub fn part(node: &StandIn, id: &str) -> Value {
    let parts = node.call("health.check", serde_json::json!({})).unwrap();
    parts["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no {id} in {parts}"))
}
