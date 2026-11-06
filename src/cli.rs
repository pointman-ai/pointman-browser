//! One agent-browser call: its argv from what an agent asked, run with a deadline and cancellable, and its answer
//! (one JSON envelope on stdout with `--json`: `{success, data, error}`) read back.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde_json::Value;

/// More output than this isn't an envelope an agent can use.
const STDOUT_MAX: u64 = 64 * 1024 * 1024;
const STDERR_MAX: u64 = 64 * 1024;

pub struct Call<'a> {
    pub bin: &'a Path,
    pub argv: &'a [String],
    /// Set for this call, over the node's own environment.
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    pub timeout: Duration,
}

#[derive(Debug, Default)]
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Run it to its end, its deadline, or `cancelled()`: then it's killed. Its output is what it wrote by the time it
/// ended: a process it left behind holding the pipes (wl-copy, owning a Wayland clipboard it set) isn't waited for.
pub fn run(call: &Call, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Ran> {
    let mut cmd = Command::new(call.bin);
    cmd.args(call.argv).current_dir(call.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // a scope from the node's own environment would send every call into someone else's session
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("AGENT_BROWSER_") {
            cmd.env_remove(name);
        }
    }
    for (k, v) in call.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", call.bin.display()))?;
    let out = Pipe::read(child.stdout.take().context("its stdout")?, STDOUT_MAX);
    let err = Pipe::read(child.stderr.take().context("its stderr")?, STDERR_MAX);
    let started = Instant::now();
    let mut ran = Ran::default();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if cancelled() {
            ran.cancelled = true;
        } else if started.elapsed() > call.timeout {
            ran.timed_out = true;
        }
        if ran.cancelled || ran.timed_out {
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(20));
    };
    ran.code = status.and_then(|s| s.code());
    let grace = Instant::now() + LINGER;
    ran.stdout = out.take(grace);
    ran.stderr = err.take(grace);
    Ok(ran)
}

/// How long output may keep coming after the CLI has ended, before what's there is taken.
const LINGER: Duration = Duration::from_millis(500);

/// One of its pipes, read on a thread into a buffer that can be taken before the pipe closes.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl Pipe {
    fn read(mut from: impl Read + Send + 'static, max: u64) -> Pipe {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (b, d) = (buf.clone(), done.clone());
        thread::spawn(move || {
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match from.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = b.lock().unwrap_or_else(|e| e.into_inner());
                        // past the most it keeps, the rest is drained, so a chatty CLI never blocks on a full pipe
                        let room = (max as usize).saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            d.store(true, Ordering::SeqCst);
        });
        Pipe { buf, done }
    }

    /// What it has: once the pipe closes, or at `by`, whichever is first.
    fn take(&self, by: Instant) -> Vec<u8> {
        while !self.done.load(Ordering::SeqCst) && Instant::now() < by {
            thread::sleep(Duration::from_millis(10));
        }
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// The envelope it printed with `--json`: `{success, data, error}`, where `error` is a message. It's returned with
/// `ok` beside `success` (the shape every Pointman CLI extension answers in), and `error` as `{message}` so an
/// agent branches on `error.message` the way it does for `desktop`. None means it crashed or timed out.
pub fn envelope(ran: &Ran) -> anyhow::Result<Value> {
    if ran.cancelled {
        bail!("cancelled");
    }
    let text = String::from_utf8_lossy(&ran.stdout);
    let found = text.trim();
    match serde_json::from_str::<Value>(found) {
        Ok(mut v) if v.get("success").is_some_and(Value::is_boolean) => {
            let ok = v["success"].as_bool().unwrap_or(false);
            v["ok"] = Value::Bool(ok);
            if let Some(msg) = v["error"].as_str().map(String::from) {
                v["error"] = serde_json::json!({ "message": msg });
            }
            Ok(v)
        }
        _ if ran.timed_out => bail!("agent-browser didn't answer in time and was stopped{}", tail(&ran.stderr)),
        _ => bail!(
            "agent-browser answered without its JSON (exit {}){}{}",
            ran.code.map_or("by a signal".to_string(), |c| c.to_string()),
            if found.is_empty() { String::new() } else { format!(": {}", clip(found, 400)) },
            tail(&ran.stderr)
        ),
    }
}

fn tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let s = s.trim();
    if s.is_empty() {
        String::new()
    } else {
        format!(". Its stderr: {}", clip_end(s, 600))
    }
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn clip_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(n - max).collect::<String>())
}

/// A session name from an agent's handle: agent-browser takes letters, digits, `-` and `_`, so the `@` and the rest
/// go. One agent, one session: its tabs, cookies and refs are its own.
pub fn session_name(handle: Option<&str>) -> String {
    let h: String = handle
        .unwrap_or_default()
        .trim()
        .trim_start_matches('@')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if h.is_empty() {
        "agent".to_string()
    } else {
        h
    }
}

/// What agents may not ask for: a way into a person's own browser, their profile, cookies and open tabs, or
/// another agent's session. Returns why, or None. `args` are the words after `agent-browser`.
pub fn refused(args: &[String]) -> Option<String> {
    const FLAGS: [&str; 10] = [
        "--profile",
        "--auto-connect",
        "--cdp",
        "--session",
        "--namespace",
        "--state",
        "--restore",
        "--session-name",
        "--provider",
        "--config",
    ];
    const COMMANDS: [&str; 8] = ["profiles", "connect", "install", "upgrade", "dashboard", "mcp", "chat", "plugin"];
    for a in args {
        let name = a.split('=').next().unwrap_or(a);
        if FLAGS.contains(&name) || name == "-p" {
            return Some(format!(
                "{name} isn't allowed: each agent has its own session and a clean browser, never a person's profile \
                 or cookies. Log in to a test account in your own session instead."
            ));
        }
    }
    match args.first().map(String::as_str) {
        Some(c) if COMMANDS.contains(&c) => Some(format!("`{c}` isn't allowed through browser.run.")),
        _ => None,
    }
}

/// One line about an answer, for whoever reads the job: what it did, or the error and what to do.
pub fn summary(command: &str, env: &Value) -> String {
    if env["ok"] != true {
        let msg = env["error"]["message"].as_str().unwrap_or("failed");
        return format!("{command}: {}", clip(msg, 300));
    }
    let d = &env["data"];
    let mut s = command.to_string();
    if let Some(title) = d["title"].as_str().filter(|t| !t.is_empty()) {
        s.push_str(&format!(": {title}"));
        if let Some(url) = d["url"].as_str() {
            s.push_str(&format!(" ({url})"));
        }
    } else if let Some(url) = d["url"].as_str() {
        s.push_str(&format!(": {url}"));
    }
    if let Some(refs) = d["refs"].as_object() {
        s.push_str(&format!(": {} refs", refs.len()));
        if let Some(o) = d["origin"].as_str() {
            s.push_str(&format!(" on {o}"));
        }
    }
    s
}

/// A screenshot answers with the `path` it wrote. Put the picture in `dir` (the job's folder, where outputs are
/// posted from) as `screenshot-<n>.<ext>`, and name it in the answer. A file already in `dir` stays as it is.
pub fn take_screenshot(env: &mut Value, command: &str, dir: &Path, n: usize) -> anyhow::Result<Option<PathBuf>> {
    if command != "screenshot" || env["ok"] != true {
        return Ok(None);
    }
    let Some(path) = env["data"]["path"].as_str().map(PathBuf::from) else { return Ok(None) };
    if !path.is_file() {
        return Ok(None);
    }
    let in_dir = path.parent().is_some_and(|p| p.canonicalize().ok() == dir.canonicalize().ok());
    if in_dir {
        env["data"]["file"] = Value::String(path.file_name().unwrap_or_default().to_string_lossy().to_string());
        return Ok(Some(path));
    }
    let ext = match path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("jpg") | Some("jpeg") => "jpg",
        _ => "png",
    };
    let name = format!("screenshot-{n}.{ext}");
    let to = dir.join(&name);
    std::fs::copy(&path, &to).with_context(|| format!("copying {}", path.display()))?;
    env["data"]["file"] = Value::String(name);
    Ok(Some(to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn words(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sessions_are_what_agent_browser_takes() {
        assert_eq!(session_name(Some("@patchbay")), "patchbay");
        assert_eq!(session_name(Some(" a b/c ")), "abc");
        assert_eq!(session_name(Some("@")), "agent");
        assert_eq!(session_name(None), "agent");
        assert_eq!(session_name(Some(&"x".repeat(80))).len(), 64);
    }

    #[test]
    fn a_persons_browser_and_other_sessions_are_refused() {
        for bad in [
            &["--profile", "Default", "open", "x"][..],
            &["open", "x", "--profile=Default"],
            &["--auto-connect", "snapshot"],
            &["--cdp", "9222", "snapshot"],
            &["--session", "other", "snapshot"],
            &["profiles"],
            &["connect", "9222"],
            &["install"],
        ] {
            assert!(refused(&words(bad)).is_some(), "{bad:?}");
        }
        for ok in [&["open", "https://example.com"][..], &["snapshot", "-i", "-c"], &["fill", "@e1", "--profile-name"]]
        {
            assert!(refused(&words(ok)).is_none(), "{ok:?}");
        }
    }

    #[test]
    fn success_becomes_ok_and_errors_carry_a_message() {
        let ran = Ran {
            stdout: br#"{"success":false,"data":null,"error":"Element not found: @nope"}"#.to_vec(),
            ..Default::default()
        };
        let v = envelope(&ran).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["message"], "Element not found: @nope");
        assert_eq!(summary("click", &v), "click: Element not found: @nope");
        let ok = Ran {
            stdout: br#"{"success":true,"data":{"title":"Example Domain","url":"https://example.com/"},"error":null}"#
                .to_vec(),
            ..Default::default()
        };
        let v = envelope(&ok).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(summary("open", &v), "open: Example Domain (https://example.com/)");
    }

    #[test]
    fn snapshots_say_how_many_refs() {
        let v = json!({"ok": true, "data": {"origin": "https://example.com/", "refs": {"e1": {}, "e2": {}}}});
        assert_eq!(summary("snapshot", &v), "snapshot: 2 refs on https://example.com/");
    }

    #[test]
    fn no_envelope_is_an_error_with_its_stderr() {
        let ran = Ran { code: Some(101), stderr: b"thread 'main' panicked".to_vec(), ..Default::default() };
        let e = envelope(&ran).unwrap_err().to_string();
        assert!(e.contains("without its JSON (exit 101)") && e.contains("panicked"), "{e}");
    }

    #[test]
    fn a_screenshot_is_copied_into_the_jobs_folder() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let src = elsewhere.path().join("abc.png");
        std::fs::write(&src, b"PNG").unwrap();
        let mut env = json!({"ok": true, "data": {"path": src.to_str().unwrap()}});
        let got = take_screenshot(&mut env, "screenshot", dir.path(), 1).unwrap().unwrap();
        assert_eq!(got, dir.path().join("screenshot-1.png"));
        assert_eq!(env["data"]["file"], "screenshot-1.png");
        let mut other = json!({"ok": true, "data": {"path": src.to_str().unwrap()}});
        assert!(take_screenshot(&mut other, "snapshot", dir.path(), 2).unwrap().is_none());
        let inside = dir.path().join("mine.png");
        std::fs::write(&inside, b"PNG").unwrap();
        let mut own = json!({"ok": true, "data": {"path": inside.to_str().unwrap()}});
        assert_eq!(take_screenshot(&mut own, "screenshot", dir.path(), 3).unwrap().unwrap(), inside);
        assert_eq!(own["data"]["file"], "mine.png");
    }
}
