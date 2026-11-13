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
prev=""
for a in "$@"; do
  if [ "$prev" = "--config" ]; then cp "$a" "$here/config.seen"; echo "$a" > "$here/config.path"; stat -c %a "$a" > "$here/config.mode" 2>/dev/null || stat -f %Lp "$a" > "$here/config.mode"; fi
  prev="$a"
done
if [ "$1" = "--version" ]; then echo "agent-browser 9.9.9"; exit 0; fi
words=""; prev=""
for a in "$@"; do
  case "$prev" in --session|--config) ;; *) case "$a" in --session|--json|--config) ;; *) words="$words $a";; esac;; esac
  prev="$a"
done
set -- $words
case "$1" in
  open) echo '{"success":true,"data":{"title":"Example Domain","url":"'"$2"'"},"error":null}';;
  auth) case "$2" in
    save) cat > "$here/stdin.log"; echo '{"success":true,"data":{"saved":"'"$3"'"},"error":null}';;
    login) if [ -f "$here/login-fails" ]; then echo '{"success":false,"data":null,"error":"login failed for hunter2 pass"}'; exit 1; fi
      echo '{"success":true,"data":{"typed":"hunter2 pass","email":"bot@example.com"},"error":null}';;
    delete) echo "deleted $3" >> "$here/deleted.log"; echo '{"success":true,"data":{},"error":null}';;
    *) echo '{"success":false,"data":null,"error":"unknown auth"}'; exit 2;;
  esac;;
  snapshot) echo '{"success":true,"data":{"origin":"https://example.com/","refs":{"e1":{"name":"Learn more","role":"link"}},"snapshot":"- link \"Learn more\" [ref=e1]"},"error":null}';;
  eval) case "$2" in
    big) printf '{"success":true,"data":{"pad":"%s"},"error":null}\n' "$(head -c 700000 /dev/zero | tr '\0' 'x')";;
      crash) echo "thread 'main' panicked" >&2; exit 101;;
      echo-typed) printf '{"success":true,"data":{"result":%s},"error":null}\n' "$(cat "$here/batch.stdin")";;
    *) echo '{"success":true,"data":{"result":"ok"},"error":null}';;
  esac;;
  click) echo '{"success":false,"data":null,"error":"Element not found: '"$2"'"}'; exit 1;;
  screenshot)
    mkdir -p "$here/shots"
    if [ -f "$here/no-browser" ]; then echo '{"success":false,"data":null,"error":"No page open"}'; exit 1; fi
    if [ -n "$2" ]; then printf '\211PNG\r\n\032\nFILE' > "$2"; echo '{"success":true,"data":{"path":"'"$2"'"},"error":null}'
    else printf '\211PNG\r\n\032\nDATA' > "$here/shots/abc.png"; echo '{"success":true,"data":{"path":"'"$here"'/shots/abc.png"},"error":null}'; fi;;
  get) if [ -f "$here/page.url" ]; then u=$(cat "$here/page.url"); else u="https://staging.example/"; fi
    echo '{"success":true,"data":{"url":"'"$u"'"},"error":null}';;
  session) echo '{"success":true,"data":{"sessions":["patchbay","deskprobe"]}}';;
  stream) p=$(cat "$here/stream.port" 2>/dev/null || echo 9)
    echo '{"success":true,"data":{"enabled":true,"port":'"$p"',"connected":true,"screencasting":false},"error":null}';;
  state) case "$2" in
    save) if [ -f "$here/state.src" ]; then cp "$here/state.src" "$3"; else echo '{"cookies":[{"name":"sid","value":"cookie-v4lue-1","domain":"stage.example.dev"}],"origins":[]}' > "$3"; fi
      echo '{"success":true,"data":{"path":"'"$3"'"},"error":null}';;
    load) cp "$3" "$here/state.loaded"; echo "$3" > "$here/state.path"; stat -c %a "$3" > "$here/state.mode" 2>/dev/null || stat -f %Lp "$3" > "$here/state.mode"
      echo '{"success":true,"data":{"skipped":[]},"error":null}';;
    *) echo '{"success":false,"data":null,"error":"unknown state"}'; exit 2;;
  esac;;
  close) echo '{"success":true,"data":{"closed":true},"error":null}';;
  batch) cat > "$here/batch.stdin"; printf '[{"index":0,"success":true,"result":{"typed":%s},"error":null}]\n' "$(cat "$here/batch.stdin")";;
  wait) sleep "$2"; echo '{"success":true,"data":{},"error":null}';;
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

/// The extension with the vault holding `secrets` (name, value) as its node would open them.
pub fn node_with_secrets(fake: &Fake, settings: Value, secrets: &[(&str, &str)]) -> StandIn {
    let home = tempfile::tempdir().unwrap();
    let node = StandIn::new(ROOT)
        .env("HOME", home.path().to_str().unwrap())
        .roots([fake.dir.path().to_path_buf()])
        .binary(env!("CARGO_BIN_EXE_browser"))
        .settings(settings)
        .machine("testbox")
        .secrets(secrets.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .env("POINTMAN_BROWSER_CLI", fake.path())
        .start()
        .unwrap();
    std::mem::forget(home);
    node
}

/// The same, with `home` as the machine person's home (where `~/lane/heavy.sh` would be).
pub fn node_in(fake: &Fake, settings: Value, home: &std::path::Path) -> StandIn {
    StandIn::new(ROOT)
        .env("HOME", home.to_str().unwrap())
        .roots([fake.dir.path().to_path_buf()])
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

/// A stand-in for agent-browser's stream (its WebSocket on 127.0.0.1): its port goes where the fake's `stream status`
/// reads it, each connection gets a 64x48 frame of a 1280x720 page every 50 ms, and the input it's sent is kept.
pub struct FakeStream {
    pub got: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    pub connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// A real 64x48 JPEG's first bytes are enough: the extension reads its size from the frame header.
pub fn tiny_jpeg() -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
    j.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 48, 0x00, 64, 0x01, 0x01, 0x11, 0x00]);
    j.extend([0xFF, 0xD9]);
    j
}

impl FakeStream {
    pub fn start(fake: &Fake) -> FakeStream {
        use base64::Engine as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(fake.dir.path().join("stream.port"), port.to_string()).unwrap();
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (g, n) = (got.clone(), connections.clone());
        std::thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let (g, n) = (g.clone(), n.clone());
                std::thread::spawn(move || {
                    let Ok(mut ws) = tungstenite::accept(tcp) else { return };
                    n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ws.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(50))).unwrap();
                    let data = base64::engine::general_purpose::STANDARD.encode(tiny_jpeg());
                    let frame = serde_json::json!({"type": "frame", "seq": 1, "data": data,
                        "metadata": {"deviceWidth": 1280, "deviceHeight": 720, "timestamp": 1785038682238u64}});
                    loop {
                        if ws.send(tungstenite::Message::text(frame.to_string())).is_err() {
                            return;
                        }
                        match ws.read() {
                            Ok(tungstenite::Message::Text(t)) => {
                                g.lock().unwrap().push(serde_json::from_str(t.as_str()).unwrap_or_default())
                            }
                            Ok(tungstenite::Message::Close(_)) => return,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(e))
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => return,
                        }
                    }
                });
            }
        });
        FakeStream { got, connections }
    }

    pub fn inputs(&self) -> Vec<Value> {
        self.got.lock().unwrap().clone()
    }

    /// Waits up to 5 s for `n` inputs.
    pub fn wait_inputs(&self, n: usize) -> Vec<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.inputs().len() < n && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.inputs()
    }
}
