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
    run_with_input(call, None, cancelled)
}

/// The same, with `input` written to its stdin (a password, which never goes in argv) and then closed.
pub fn run_with_input(call: &Call, input: Option<&[u8]>, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Ran> {
    let mut cmd = Command::new(call.bin);
    cmd.args(call.argv)
        .current_dir(call.cwd)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        use std::io::Write;
        // a CLI that exits without reading is its own answer: the write's error is not
        let _ = stdin.write_all(bytes);
    }
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

/// The most a number may be in a Pointman answer: core refuses any number past ±2^53.
const MAX_WHOLE: u64 = 1 << 53;

/// Numbers past ±2^53 (agent-browser's `launchHash` is a random u64) become strings: Pointman's core refuses the
/// whole job answer with a 400 otherwise.
pub fn stringify_big_numbers(v: &mut Value) {
    match v {
        Value::Number(n)
            if n.as_i64()
                .map(|i| i.unsigned_abs() > MAX_WHOLE)
                .or(n.as_u64().map(|u| u > MAX_WHOLE))
                .unwrap_or_else(|| n.as_f64().is_some_and(|f| f.abs() > MAX_WHOLE as f64)) =>
        {
            *v = Value::String(n.to_string())
        }
        Value::Array(a) => a.iter_mut().for_each(stringify_big_numbers),
        Value::Object(o) => o.values_mut().for_each(stringify_big_numbers),
        _ => {}
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
            stringify_big_numbers(&mut v);
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
/// go. One agent, one session: its tabs, cookies and refs are its own. A handle with nothing left is None: it would
/// share one session with every other such handle.
pub fn session_name(handle: Option<&str>) -> Option<String> {
    let h: String = handle
        .unwrap_or_default()
        .trim()
        .trim_start_matches('@')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    (!h.is_empty()).then_some(h)
}

/// The commands browser.run passes on. Anything else, including what a later agent-browser adds, is refused.
const COMMANDS: [&str; 30] = [
    "open",
    "snapshot",
    "click",
    "dblclick",
    "fill",
    "type",
    "press",
    "keyboard",
    "hover",
    "focus",
    "select",
    "check",
    "uncheck",
    "scroll",
    "scrollintoview",
    "wait",
    "get",
    "is",
    "find",
    "tab",
    "back",
    "forward",
    "reload",
    "screenshot",
    "eval",
    "close",
    "console",
    "errors",
    "highlight",
    "mouse",
];
/// Flags that stand alone.
const BOOL_FLAGS: [&str; 15] = [
    "--json",
    "--headed",
    "--full",
    "-f",
    "--if-changed",
    "--annotate",
    "-i",
    "--interactive",
    "-c",
    "--compact",
    "--exact",
    "--clear",
    "--no-auto-dialog",
    "--content-boundaries",
    "--pin-tab",
];
/// Flags that take the next word (or `=value`). None of them names a file.
const VALUE_FLAGS: [&str; 17] = [
    "--threshold",
    "--screenshot-format",
    "--screenshot-quality",
    "--timeout",
    "--url",
    "--load",
    "--fn",
    "--text",
    "--state",
    "--depth",
    "-d",
    "--selector",
    "-s",
    "--max-output",
    "--name",
    "--nth",
    "--filter",
];
/// Schemes that reach the machine itself or run code, whatever the command.
const BAD_SCHEMES: [&str; 8] =
    ["file", "chrome", "chrome-extension", "javascript", "data", "blob", "filesystem", "view-source"];

fn is_number(a: &str) -> bool {
    a.parse::<f64>().is_ok()
}

/// A word that begins a flag: `-x` or `--x`, not a negative number.
fn is_flag(a: &str) -> bool {
    a.starts_with('-') && a.len() > 1 && !is_number(a)
}

/// The words that aren't flags or flag values, and the first flag that isn't allowed.
fn split_words(args: &[String]) -> Result<Vec<&str>, String> {
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if is_flag(a) {
            let (name, inline) = match a.split_once('=') {
                Some((n, _)) => (n, true),
                None => (a, false),
            };
            if BOOL_FLAGS.contains(&name) {
                // `--headed false`: a word that is true or false belongs to the flag
                if !inline && args.get(i + 1).is_some_and(|v| v == "true" || v == "false") {
                    i += 1;
                }
            } else if VALUE_FLAGS.contains(&name) {
                if !inline {
                    i += 1;
                }
            } else {
                return Err(format!(
                    "{name} isn't allowed through browser.run. Allowed flags: {}.",
                    BOOL_FLAGS.iter().chain(VALUE_FLAGS.iter()).copied().collect::<Vec<_>>().join(" ")
                ));
            }
        } else {
            words.push(a);
        }
        i += 1;
    }
    Ok(words)
}

/// Whether the words ask for a visible window: `--headed`, `--headed=true`, `--headed true`, not `--headed=false`.
pub fn wants_headed(args: &[String]) -> bool {
    for (i, a) in args.iter().enumerate() {
        let (name, value) = match a.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (a.as_str(), args.get(i + 1).map(String::as_str).filter(|v| *v == "true" || *v == "false")),
        };
        if name == "--headed" && value != Some("false") {
            return true;
        }
    }
    false
}

/// A path an agent names for a file agent-browser writes: inside `work` (the job's folder), and one the node allows.
/// Returns the path to give agent-browser.
fn work_path(given: &str, work: &Path, allowed: &dyn Fn(&Path) -> anyhow::Result<PathBuf>) -> Result<PathBuf, String> {
    let p = Path::new(given);
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) || given.starts_with('~') {
        return Err(format!("{given} isn't allowed: a file goes in the job's own folder, with no `..`."));
    }
    let full = if p.is_absolute() { p.to_path_buf() } else { work.join(p) };
    let ok = allowed(&full).map_err(|e| format!("{given} isn't allowed: {e:#}"))?;
    let root = allowed(work).map_err(|e| format!("the job's folder: {e:#}"))?;
    if !ok.starts_with(&root) {
        return Err(format!("{given} isn't allowed: a file goes in the job's own folder."));
    }
    Ok(full)
}

/// How agent-browser 0.41.2 tells `screenshot <word>`'s path from its selector.
fn looks_like_path(w: &str) -> bool {
    let l = w.to_ascii_lowercase();
    l.starts_with("./")
        || l.starts_with("../")
        || l.contains('/')
        || [".png", ".jpg", ".jpeg", ".webp"].iter().any(|e| l.ends_with(e))
}

/// What agents may not ask for, as an allowlist of commands and flags: nothing reaches a person's browser, profile or
/// files, another agent's session, or the machine through a new agent-browser feature. Returns why, or None. `args`
/// are the words after `agent-browser`; a path a command writes must be inside `work` and one `allowed` lets in.
pub fn refused(args: &[String], work: &Path, allowed: &dyn Fn(&Path) -> anyhow::Result<PathBuf>) -> Option<String> {
    if args.iter().any(|a| a.to_ascii_lowercase().replace(['%', ' '], "").contains("file:")) {
        return Some("file: isn't allowed: browse the web, not this machine's files.".to_string());
    }
    let words = match split_words(args) {
        Ok(w) => w,
        Err(e) => return Some(e),
    };
    let Some(&command) = words.first() else { return Some("name a command: open, snapshot, click…".to_string()) };
    if !COMMANDS.contains(&command) {
        return Some(format!("`{command}` isn't allowed through browser.run. Allowed: {}.", COMMANDS.join(" ")));
    }
    // a word that opens something that isn't the web
    for w in &words {
        let l = w.trim_start().to_ascii_lowercase();
        for scheme in BAD_SCHEMES {
            if l.strip_prefix(scheme)
                .and_then(|r| r.strip_prefix(':'))
                .is_some_and(|r| !r.starts_with(char::is_whitespace))
            {
                return Some(format!("{scheme}: isn't allowed: browse the web, not this machine."));
            }
        }
    }
    // `--state` is a wait's element state here; elsewhere it restores a saved browser state from a file
    for (i, a) in args.iter().enumerate() {
        let value = match a.split_once('=') {
            Some(("--state", v)) => Some(v),
            _ if a == "--state" => args.get(i + 1).map(String::as_str),
            _ => continue,
        };
        if command != "wait" || !matches!(value, Some("visible" | "hidden" | "attached" | "detached")) {
            return Some("--state isn't allowed except as a wait's visible, hidden, attached or detached.".to_string());
        }
    }
    if command == "get" && words.get(1) == Some(&"cdp-url") {
        return Some("`get cdp-url` isn't allowed: it hands out a way into the browser.".to_string());
    }
    if command == "screenshot" {
        let rest = &words[1..];
        let path = match rest {
            [] => None,
            // agent-browser's own rule for one word: a path if it starts with ./ or ../, has a /, or ends in an image
            [one] => looks_like_path(one).then_some(*one),
            [_, path] => Some(*path),
            _ => return Some("screenshot takes a selector and a path, at most.".to_string()),
        };
        if let Some(p) = path {
            if let Err(e) = work_path(p, work, allowed) {
                return Some(e);
            }
        }
    }
    None
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
        assert_eq!(session_name(Some("@patchbay")).as_deref(), Some("patchbay"));
        assert_eq!(session_name(Some(" a b/c ")).as_deref(), Some("abc"));
        assert_eq!(session_name(Some("@")), None, "a handle with nothing left shares no session");
        assert_eq!(session_name(Some("///")), None);
        assert_eq!(session_name(None), None);
        assert_eq!(session_name(Some(&"x".repeat(80))).map(|s| s.len()), Some(64));
    }

    fn deny(args: &[&str]) -> Option<String> {
        let work = std::env::temp_dir().join("ab-refused-work");
        let w = work.clone();
        let allowed = move |p: &Path| -> anyhow::Result<PathBuf> {
            if p.starts_with(&w) {
                Ok(p.to_path_buf())
            } else {
                bail!("{} is outside the folders this node lets extensions use", p.display())
            }
        };
        refused(&words(args), &work, &allowed)
    }

    #[test]
    fn a_persons_browser_and_other_sessions_are_refused() {
        for bad in [
            &["--profile", "Default", "open", "x"][..],
            &["open", "x", "--profile=Default"],
            &["--auto-connect", "snapshot"],
            &["--cdp", "9222", "snapshot"],
            &["--session", "other", "snapshot"],
            &["--state", "/Users/me/state.json", "open", "x"],
            &["--config", "/tmp/c.json", "open", "x"],
            &["--executable-path", "/bin/sh", "open", "x"],
            &["--extension", "/tmp/e", "open", "x"],
            &["--init-script", "/tmp/s.js", "open", "x"],
            &["--allow-file-access", "open", "x"],
            &["--args", "--no-sandbox", "open", "x"],
            &["--proxy", "http://x", "open", "y"],
            &["--download-path", "/tmp", "open", "x"],
            &["close", "--all"],
            &["profiles"],
            &["connect", "9222"],
            &["install"],
            &["upgrade"],
            &["dashboard"],
            &["skills", "get", "core"],
            &["auth", "save", "x"],
            &["cookies", "get"],
            &["storage", "local"],
            &["clipboard", "read"],
            &["upload", "@e1", "/etc/passwd"],
            &["download", "@e1", "/tmp/x"],
            &["pdf", "/tmp/x.pdf"],
            &["record", "start", "/tmp/x.webm"],
            &["trace", "stop", "/tmp/t"],
            &["network", "har", "stop", "/tmp/h"],
            &["wait", "--download", "/tmp/x"],
            &["get", "cdp-url"],
            &["batch", "open x"],
        ] {
            assert!(deny(bad).is_some(), "{bad:?}");
        }
        for ok in [
            &["open", "https://example.com"][..],
            &["open", "localhost:3000"],
            &["snapshot", "-i", "-c"],
            &["fill", "@e1", "profile-name"],
            &["fill", "@e1", "-5"],
            &["wait", "--load", "networkidle"],
            &["wait", "#spinner", "--state", "hidden"],
            &["get", "text", "@e1"],
            &["find", "role", "button", "click", "--name", "Go"],
            &["tab", "new", "https://example.com"],
            &["screenshot"],
            &["screenshot", "--full", "page.png"],
            &["screenshot", "#main", "shots/a.png"],
            &["eval", "document.title"],
            &["close"],
            &["open", "https://example.com", "--json", "--headed=false"],
        ] {
            assert_eq!(deny(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn local_files_and_urls_that_run_code_are_refused() {
        for bad in [
            &["open", "file:///etc/passwd"][..],
            &["open", "FILE:///etc/passwd"],
            &["tab", "new", "file:///Users/x"],
            &["eval", "location='file:///etc/hosts'"],
            &["open", "chrome://settings"],
            &["open", "javascript:alert(1)"],
            &["open", "data:text/html,<script>1</script>"],
            &["open", "view-source:https://x"],
        ] {
            assert!(deny(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn a_screenshot_goes_in_the_jobs_folder() {
        let work = std::env::temp_dir().join("ab-refused-work");
        let inside = work.join("a.png");
        for bad in [
            &["screenshot", "/Users/me/Desktop/a.png"][..],
            &["screenshot", "/etc/cron.d/x.png"],
            &["screenshot", "../a.png"],
            &["screenshot", "a/../../b.png"],
            &["screenshot", "~/a.png"],
            &["screenshot", "#main", "/tmp/out.png"],
            &["screenshot", "/Users/james/.zshrc"],
            &["screenshot", "../../../.zshrc"],
            &["screenshot", "./../x"],
            &["screenshot", "/etc/hosts"],
        ] {
            assert!(deny(bad).is_some(), "{bad:?}");
        }
        assert_eq!(deny(&["screenshot", inside.to_str().unwrap()]), None);
        assert_eq!(deny(&["screenshot", "a.png"]), None);
        assert_eq!(deny(&["screenshot", "dir/file"]), None, "inside the job's folder");
        assert_eq!(deny(&["screenshot", "#main > a"]), None, "a selector, not a path");
    }

    #[test]
    fn a_visible_window_is_asked_for_by_any_spelling() {
        for yes in [&["open", "x", "--headed"][..], &["--headed=true", "open", "x"], &["--headed", "true", "open", "x"]]
        {
            assert!(wants_headed(&words(yes)), "{yes:?}");
        }
        for no in [&["open", "x"][..], &["--headed=false", "open", "x"], &["--headed", "false", "open", "x"]] {
            assert!(!wants_headed(&words(no)), "{no:?}");
        }
    }

    #[test]
    fn numbers_past_signed_64_bits_become_strings() {
        let ran = Ran {
            code: Some(0),
            stdout: br#"{"success":true,"data":{"lifecycle":{"effectiveLaunch":{"launchHash":11677035664275010974,"n":5}}},"error":null}"#.to_vec(),
            ..Default::default()
        };
        let env = envelope(&ran).unwrap();
        assert_eq!(env["data"]["lifecycle"]["effectiveLaunch"]["launchHash"], "11677035664275010974");
        assert_eq!(env["data"]["lifecycle"]["effectiveLaunch"]["n"], 5);
    }

    #[test]
    fn numbers_past_2_pow_53_become_strings_and_the_rest_stay() {
        let ran = Ran {
            code: Some(0),
            stdout: br#"{"success":true,"data":{"a":9007199254740993,"b":-9007199254740993,"c":9007199254740992,"d":-9007199254740992,"e":9223372036854775807,"f":-9223372036854775808,"g":1.5,"h":1e20}}"#.to_vec(),
            ..Default::default()
        };
        let d = &envelope(&ran).unwrap()["data"];
        assert_eq!(d["a"], "9007199254740993");
        assert_eq!(d["b"], "-9007199254740993");
        assert_eq!(d["c"], 9007199254740992u64);
        assert_eq!(d["d"], -9007199254740992i64);
        assert_eq!(d["e"], "9223372036854775807");
        assert_eq!(d["f"], "-9223372036854775808");
        assert_eq!(d["g"], 1.5);
        assert!(d["h"].is_string(), "{}", d["h"]);
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
