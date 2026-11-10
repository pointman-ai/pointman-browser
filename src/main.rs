//! The extension's process: the node starts it (`bin/browser`, from the extension's folder) and talks to it over
//! stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! `browser.run` runs one agent-browser command on this machine for an agent anywhere (create_job with `machine`),
//! in a session named for that agent: its own tabs, cookies and refs, in a clean headless Chrome that's never a
//! person's browser. Each call runs on a thread of its own and can be cancelled: the CLI is killed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use browser::cli::{self, Call};
use browser::{install, test};
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::{HealthCheckRequest, HealthCheckResponse, HealthFixRequest, HealthFixResponse};
use pointman_extension::v1::health::part::State;
use pointman_extension::v1::health::{Action, Part};
use pointman_extension::{Extension, Handle, Job};
use serde_json::{json, Value};

const DEFAULT_TIMEOUT: u64 = 60;
const MAX_TIMEOUT: u64 = 600;
/// The extension's own calls (status, installing Chrome is longer: see CHROME_INSTALL).
const QUICK: Duration = Duration::from_secs(30);
const CHROME_INSTALL: Duration = Duration::from_secs(900);
/// An answer bigger than this goes back as a file, with the envelope's small parts.
const ANSWER_MAX: usize = 512 * 1024;
/// Where Linux keeps a system Chromium, when the `chrome` setting names none.
const SYSTEM_CHROME: [&str; 5] = ["chromium", "chromium-browser", "google-chrome-stable", "google-chrome", "chrome"];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Ext {
    h: Handle,
    /// The last install's error; held while one runs.
    installing: Mutex<Option<String>>,
    chrome_installing: Mutex<Option<String>>,
    /// One run's file is written by one call at a time.
    runs: Mutex<()>,
    counter: AtomicU64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A test run is kept this long after its last step.
const RUN_DAYS: u64 = 7;
/// Waiting for the heavy-job lane gives up after this.
const LANE_WAIT: Duration = Duration::from_secs(30 * 60);
/// A run holds the lane at most this long: a crashed or abandoned one is let go by the sweeper.
const LANE_HOLD_MAX: u64 = 30 * 60;

/// The job text the lane sees. Its script gives a job whose words say "window" or "keys-smoke" both lanes, alone, so
/// a run's name has those words spelled out of it.
fn lane_job(name: &str) -> String {
    let mut out = String::new();
    let mut rest = name;
    while !rest.is_empty() {
        let low = rest.to_ascii_lowercase();
        let hit = ["keys-smoke", "window"].iter().filter_map(|w| low.find(w).map(|i| (i, w.len()))).min();
        let Some((i, len)) = hit else { break };
        out.push_str(&rest[..i]);
        out.push_str(if len == 6 { "win-dow" } else { "keys smoke" });
        rest = &rest[i + len..];
    }
    out.push_str(rest);
    format!("UI test: {out}")
}

impl Ext {
    fn state(&self) -> PathBuf {
        let s = self.h.state_dir();
        if s.is_absolute() {
            s
        } else {
            std::env::current_dir().map(|d| d.join(&s)).unwrap_or(s)
        }
    }

    fn setting(&self, name: &str, default: bool) -> bool {
        self.h.settings()[name].as_bool().unwrap_or(default)
    }

    /// The heavy-job lock's script on this machine (the `lock` setting), if it has one: a test run holds its lane.
    fn lane(&self) -> Option<PathBuf> {
        self.h.settings()["lock"].as_str().map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from)
    }

    /// `sh <script> <verb> <holder> …`, to its end.
    fn lane_call(
        &self,
        script: &Path,
        args: &[&str],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<cli::Ran> {
        let mut argv = vec![script.to_string_lossy().to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let home = self.home()?;
        let call = Call { bin: Path::new("sh"), argv: &argv, env: &[], cwd: &home, timeout };
        cli::run(&call, cancelled)
    }

    /// Take a lane and keep it until `release_lane`: `take` waits its turn, and after 9 minutes exits 3 and keeps the
    /// place, so ask again.
    fn take_lane(&self, script: &Path, holder: &str, job: &str, cancelled: &dyn Fn() -> bool) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        loop {
            let ran = self.lane_call(script, &["take", holder, job], Duration::from_secs(11 * 60), cancelled)?;
            if ran.cancelled {
                let _ = self.release_lane(script, holder);
                bail!(pointman_extension::Cancelled);
            }
            match ran.code {
                Some(0) => return Ok(()),
                Some(3) if started.elapsed() < LANE_WAIT => continue,
                code => bail!(
                    "the heavy-job lane wasn't free ({}): {}",
                    code.map_or("no exit code".to_string(), |c| format!("exit {c}")),
                    cli::clip(String::from_utf8_lossy(&ran.stderr).trim(), 300)
                ),
            }
        }
    }

    fn release_lane(&self, script: &Path, holder: &str) -> anyhow::Result<()> {
        self.lane_call(script, &["release", holder], QUICK, &|| false).map(|_| ())
    }

    /// The CLI: the pinned release, installed now if it isn't yet. POINTMAN_BROWSER_CLI points the tests at a fake.
    fn cli(&self) -> anyhow::Result<PathBuf> {
        if let Some(p) = std::env::var_os("POINTMAN_BROWSER_CLI").filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(p));
        }
        let state = self.state();
        if let Some(p) = install::installed(&state) {
            return Ok(p);
        }
        let mut last = lock(&self.installing);
        if let Some(p) = install::installed(&state) {
            return Ok(p);
        }
        let releases = std::env::var("POINTMAN_BROWSER_RELEASES").unwrap_or_else(|_| install::RELEASES.to_string());
        match install::install(&state, &releases) {
            Ok(p) => {
                *last = None;
                self.h.log(format!("installed agent-browser {} at {}", install::VERSION, p.display()));
                Ok(p)
            }
            Err(e) => {
                *last = Some(format!("{e:#}"));
                Err(e.context(format!("installing agent-browser {}", install::VERSION)))
            }
        }
    }

    /// agent-browser's own home: its Chrome, sessions and saved state, in the extension's folder, private. Its HOME
    /// for every call, so nothing of the machine's person (their `~/.agent-browser`, their Chrome) is read.
    fn home(&self) -> anyhow::Result<PathBuf> {
        let home = self.state().join("home");
        private_dir(&home)?;
        Ok(home)
    }

    /// Where its sockets go: short, since a unix socket's path is limited to about 100 characters.
    fn sockets(&self) -> anyhow::Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let id = format!("{:x}", Sha256::digest(self.state().to_string_lossy().as_bytes()));
        let dir = PathBuf::from("/tmp").join(format!("pbr-{}", &id[..8]));
        private_dir(&dir)?;
        Ok(dir)
    }

    /// The Chrome to use on Linux: the `chrome` setting, else one on the PATH (NixOS can't run a downloaded Chrome).
    fn system_chrome(&self) -> Option<PathBuf> {
        if let Some(p) = self.h.settings()["chrome"].as_str().filter(|p| !p.trim().is_empty()) {
            return Some(PathBuf::from(p.trim()));
        }
        if !cfg!(target_os = "linux") {
            return None;
        }
        if let Some(p) = self.nix_chrome() {
            return Some(p);
        }
        which(&SYSTEM_CHROME)
    }

    /// The Chromium this extension built on NixOS: `<state>/chromium/bin/chromium`, a GC root in its own folder.
    fn nix_chrome(&self) -> Option<PathBuf> {
        let p = nix_chrome_bin(&self.state());
        p.is_file().then_some(p)
    }

    /// NixOS can't run a downloaded Chrome and the person installs nothing: build nixpkgs' Chromium into the state
    /// folder the first time, unless the `chrome` setting names one or the out-link is there.
    fn ensure_nix_chrome(&self) -> anyhow::Result<()> {
        let set = self.h.settings()["chrome"].as_str().is_some_and(|p| !p.trim().is_empty());
        if set || !is_nixos() || self.nix_chrome().is_some() {
            return Ok(());
        }
        let mut last = lock(&self.chrome_installing);
        if self.nix_chrome().is_some() {
            return Ok(());
        }
        let nix =
            which(&["nix"]).context("this is NixOS but `nix` isn't on the node's PATH, so Chromium can't be built")?;
        let home = self.home()?;
        let link = nix_chrome_link(&self.state());
        let argv: Vec<String> = [
            "build",
            "nixpkgs#chromium",
            "--out-link",
            &link.to_string_lossy(),
            "--extra-experimental-features",
            "nix-command flakes",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let call = Call { bin: &nix, argv: &argv, env: &[], cwd: &home, timeout: CHROME_INSTALL };
        let ran = cli::run(&call, &|| false)?;
        if self.nix_chrome().is_some() {
            *last = None;
            self.h.log(format!("built Chromium at {}", link.display()));
            return Ok(());
        }
        let why = format!(
            "nix couldn't build Chromium (exit {}): {}",
            ran.code.map_or("signal".into(), |c| c.to_string()),
            cli::clip(String::from_utf8_lossy(&ran.stderr).trim(), 400)
        );
        *last = Some(why.clone());
        bail!(why)
    }

    fn env(&self, home: &Path) -> anyhow::Result<Vec<(String, String)>> {
        let mut env = vec![
            ("HOME".to_string(), home.to_string_lossy().to_string()),
            ("AGENT_BROWSER_SOCKET_DIR".to_string(), self.sockets()?.to_string_lossy().to_string()),
            ("AGENT_BROWSER_NO_VERSION_CHECK".to_string(), "1".to_string()),
        ];
        if let Some(c) = self.system_chrome() {
            env.push(("AGENT_BROWSER_EXECUTABLE_PATH".to_string(), c.to_string_lossy().to_string()));
        }
        Ok(env)
    }

    /// One of the extension's own calls, in its scratch folder, read as an envelope.
    fn quick(&self, bin: &Path, home: &Path, argv: &[&str]) -> anyhow::Result<Value> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let env = self.env(home)?;
        let call = Call { bin, argv: &argv, env: &env, cwd: home, timeout: QUICK };
        cli::envelope(&cli::run(&call, &|| false)?)
    }

    /// The Chrome agent-browser downloads (`agent-browser install`), unless a system one is used or it's there.
    fn ensure_chrome(&self) -> anyhow::Result<()> {
        if std::env::var_os("POINTMAN_BROWSER_CLI").is_none() {
            self.ensure_nix_chrome()?;
        }
        if self.system_chrome().is_some() || std::env::var_os("POINTMAN_BROWSER_CLI").is_some() {
            return Ok(());
        }
        let home = self.home()?;
        if downloaded_chrome(&home).is_some() {
            return Ok(());
        }
        let bin = self.cli()?;
        let mut last = lock(&self.chrome_installing);
        if downloaded_chrome(&home).is_some() {
            return Ok(());
        }
        let env = self.env(&home)?;
        let argv = vec!["install".to_string()];
        let call = Call { bin: &bin, argv: &argv, env: &env, cwd: &home, timeout: CHROME_INSTALL };
        let ran = cli::run(&call, &|| false)?;
        if downloaded_chrome(&home).is_some() {
            *last = None;
            self.h.log("installed Chrome for agent-browser");
            return Ok(());
        }
        let why = format!(
            "agent-browser couldn't install Chrome (exit {}): {}",
            ran.code.map_or("signal".into(), |c| c.to_string()),
            cli::clip(String::from_utf8_lossy(&ran.stderr).trim(), 400)
        );
        *last = Some(why.clone());
        bail!(why)
    }
}

/// The out-link `nix build` makes in the state folder.
fn nix_chrome_link(state: &Path) -> PathBuf {
    state.join("chromium")
}

fn nix_chrome_bin(state: &Path) -> PathBuf {
    nix_chrome_link(state).join("bin/chromium")
}

/// NixOS marks itself with /etc/NIXOS (POINTMAN_BROWSER_NIXOS=1 or 0 overrides it, for tests).
fn is_nixos() -> bool {
    match std::env::var("POINTMAN_BROWSER_NIXOS").ok().as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => Path::new("/etc/NIXOS").exists(),
    }
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn which(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    // a node run as a service has a short PATH: NixOS's system and per-user bin folders are where Chromium is
    dirs.push("/run/current-system/sw/bin".into());
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".nix-profile/bin"));
    }
    for d in dirs {
        for n in names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The Chrome agent-browser downloaded into `home`: `<home>/.agent-browser/browsers/chrome-<version>`.
fn downloaded_chrome(home: &Path) -> Option<String> {
    let dir = home.join(".agent-browser/browsers");
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("chrome-"))
        .collect();
    found.sort();
    found.pop().map(|n| n.trim_start_matches("chrome-").to_string())
}

fn strings(v: &Value, name: &str) -> anyhow::Result<Vec<String>> {
    let Some(list) = v.as_array() else {
        bail!("{name} is the words after `agent-browser`, as a list: [\"open\", \"https://example.com\"]")
    };
    list.iter()
        .map(|a| a.as_str().map(String::from).context(format!("{name} has something that isn't a word")))
        .collect()
}

/// browser.run: one agent-browser command in the agent's own session, its envelope back, a screenshot posted.
fn run(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let args = strings(&job.params["args"], "args")?;
    if args.is_empty() {
        bail!("args is empty: the words after `agent-browser`, [\"open\", \"https://example.com\"]");
    }
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    if let Some(why) = cli::refused(&args, &job.work, &|p: &Path| x.h.allowed(p)) {
        bail!(why);
    }
    if cli::wants_headed(&args) && !x.setting("headed", false) {
        bail!(
            "A visible browser window is off on {} (the Browser extension's \"Visible window\" setting): browse \
             headless, or ask this machine's person to turn it on.",
            x.h.machine()
        );
    }
    let agent = cli::session_name(job.params["agent"].as_str()).context(
        "agent is your handle, with letters or digits: each agent has its own browser session, named for it",
    )?;
    let timeout = Duration::from_secs(job.params["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT));

    job.progress(None, "agent-browser");
    let bin = x.cli()?;
    x.ensure_chrome()?;
    let home = x.home()?;
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    let before = files_in(&job.work);

    let (mut env, command) = call(x, &bin, &home, &agent, &args, &job.work, timeout, &|| job.cancelled())?;
    if env["error"].is_null() {
        env["error"] = Value::Null;
    }

    // what it made: a screenshot's picture, and any file it wrote where it ran
    let n = before.len() + 1;
    let shot = cli::take_screenshot(&mut env, &command, &job.work, n)?;
    let made: Vec<PathBuf> = files_in(&job.work).into_iter().filter(|f| !before.contains(f)).collect();
    for f in &made {
        let image = matches!(ext(f).as_str(), "png" | "jpg" | "jpeg");
        job.output(f, if image { "image" } else { "document" }, Role::Main, json!({"file": name_of(f)}));
    }
    let text = cli::summary(&command, &env);
    let mut answer = env;
    let size = answer.to_string().len();
    if size > ANSWER_MAX {
        let name = format!("{}.json", file_stem(&command));
        let path = job.work.join(&name);
        std::fs::write(&path, serde_json::to_vec_pretty(&answer)?)?;
        job.output(&path, "document", Role::Main, json!({"file": name}));
        answer["data"] = json!({"file": name, "bytes": size, "note": "Too big to answer inline, so it's posted as a \
            file. Narrow it: snapshot -i -c, or -s <css selector>, or get text <selector>."});
    }
    answer["text"] = Value::String(match shot {
        Some(p) => format!("{text}: posted {}", name_of(&p)),
        None => text,
    });
    answer["session"] = Value::String(agent);
    answer["machine"] = Value::String(x.h.machine());
    Ok(answer)
}

/// One call in `agent`'s session: `agent-browser --session <agent> --json <args>`. Returns its envelope and the
/// command's name (the first word that isn't a flag).
#[allow(clippy::too_many_arguments)]
fn call(
    x: &Ext,
    bin: &Path,
    home: &Path,
    agent: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<(Value, String)> {
    let mut argv = vec!["--session".to_string(), agent.to_string(), "--json".to_string()];
    argv.extend(args.iter().cloned());
    let vars = x.env(home)?;
    let c = Call { bin, argv: &argv, env: &vars, cwd, timeout };
    let ran = cli::run(&c, cancelled)?;
    if ran.cancelled {
        bail!(pointman_extension::Cancelled);
    }
    let env = cli::envelope(&ran)?;
    let command = args.iter().find(|a| !a.starts_with('-')).cloned().unwrap_or_else(|| "agent-browser".into());
    Ok((env, command))
}

/// A file name made from the command: only `[a-z0-9-]`, so it stays in the job's folder.
fn file_stem(command: &str) -> String {
    let stem: String = command
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(40)
        .collect();
    if stem.is_empty() {
        "answer".into()
    } else {
        stem
    }
}

/// browser.status: agent-browser's version, the Chrome it uses, and the open sessions.
fn status(x: &Ext, _job: &Job) -> anyhow::Result<Value> {
    let bin = x.cli()?;
    let home = x.home()?;
    let sessions = x.quick(&bin, &home, &["session", "list", "--json"]).ok();
    let list: Vec<Value> =
        sessions.as_ref().and_then(|v| v["data"]["sessions"].as_array().cloned()).unwrap_or_default();
    let chrome = match x.system_chrome() {
        Some(p) => json!({"kind": "system", "path": p.to_string_lossy()}),
        None => match downloaded_chrome(&home) {
            Some(v) => json!({"kind": "downloaded", "version": v}),
            None => json!({"kind": "none"}),
        },
    };
    let version =
        if std::env::var_os("POINTMAN_BROWSER_CLI").is_some() { version_of(&bin) } else { install::VERSION.into() };
    let text = format!(
        "agent-browser {version} on {}: Chrome {}, {} open session{}",
        x.h.machine(),
        match chrome["kind"].as_str() {
            Some("system") => format!("(system) {}", chrome["path"].as_str().unwrap_or("")),
            Some("downloaded") => chrome["version"].as_str().unwrap_or("?").to_string(),
            _ => "not installed yet".into(),
        },
        list.len(),
        if list.len() == 1 { "" } else { "s" }
    );
    Ok(json!({
        "ok": true,
        "data": {"version": version, "chrome": chrome, "sessions": list, "headed_allowed": x.setting("headed", false)},
        "text": text,
        "machine": x.h.machine(),
    }))
}

fn version_of(bin: &Path) -> String {
    let out = std::process::Command::new(bin).arg("--version").output();
    out.ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_start_matches("agent-browser ").to_string())
        .unwrap_or_else(|| "?".into())
}

/// browser.test_start: a run from a checklist, holding the machine's heavy-job lane (where it has one) to `finish`.
fn test_start(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let name = job.params["name"].as_str().map(str::trim).filter(|n| !n.is_empty());
    let name = name.context("name is what the run is called: \"Spacesheep staging: sign in\"")?;
    let markdown = job.params["checklist"].as_str().context("checklist is markdown: one `- ` line a step")?;
    let steps = test::parse_checklist(markdown)?;
    let agent = cli::session_name(job.params["agent"].as_str())
        .context("agent is your handle, with letters or digits: the run uses your own browser session")?;
    let state = x.state();
    test_prune(&state);
    // a run this agent left open gives up its lane first
    let _ = test_abandon(x, &state, &agent);

    let id = format!("run-{}-{}", now(), x.counter.fetch_add(1, Ordering::SeqCst) + 1);
    let mut run = test::new_run(&id, name, &agent, &x.h.machine(), now(), &steps);
    if x.lane().is_none() {
        // a machine that has the lane script but no `lock` setting would run beside other jobs without anyone noticing
        if let Some(p) =
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("lane/heavy.sh")).filter(|p| p.is_file())
        {
            bail!("this machine has a heavy-job lock at {} but the `lock` setting is empty: set `lock` to it, so a run holds its lane", p.display());
        }
    }
    if let Some(script) = x.lane() {
        job.progress(None, "waiting for the heavy-job lane");
        x.take_lane(&script, &agent, &lane_job(name), &|| job.cancelled())?;
        run["lane"] = json!(script.to_string_lossy());
        run["lane_at"] = json!(now());
    }
    let dir = test::run_dir(&state, &id);
    if let Err(e) = private_dir(&dir).and_then(|_| save_run(&dir, &run)) {
        if let Some(script) = x.lane() {
            let _ = x.release_lane(&script, &agent);
        }
        return Err(e);
    }
    let text = format!(
        "{id}: {} step{} to run{}",
        steps.len(),
        if steps.len() == 1 { "" } else { "s" },
        if run["lane"].is_null() { "" } else { ", holding the heavy-job lane until finish" }
    );
    Ok(
        json!({"ok": true, "run": id, "steps": steps.iter().map(|s| json!({"text": s.text, "expect": s.expect})).collect::<Vec<_>>(), "text": text, "machine": x.h.machine()}),
    )
}

fn save_run(dir: &Path, run: &Value) -> anyhow::Result<()> {
    let tmp = dir.join(".run.json.part");
    std::fs::write(&tmp, serde_json::to_vec_pretty(run)?)?;
    std::fs::rename(&tmp, dir.join("run.json"))?;
    Ok(())
}

fn load_run(state: &Path, id: &str) -> anyhow::Result<(PathBuf, Value)> {
    if !test::valid_run_id(id) {
        bail!("run is the id browser.test_start gave: run-<number>-<number>");
    }
    let dir = test::run_dir(state, id);
    let text = std::fs::read_to_string(dir.join("run.json"))
        .with_context(|| format!("no run {id} here: start one with browser.test_start (on this machine)"))?;
    Ok((dir.clone(), serde_json::from_str(&text).with_context(|| format!("{id}'s record is damaged"))?))
}

/// An unfinished run of `agent`'s is closed, and its lane let go.
fn test_abandon(x: &Ext, state: &Path, agent: &str) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(state.join("runs")) else { return Ok(()) };
    for e in entries.flatten() {
        let Some(id) = e.file_name().to_str().map(String::from) else { continue };
        let Ok((dir, mut run)) = load_run(state, &id) else { continue };
        if run["agent"] == agent && run["finished"].is_null() {
            if let Some(script) = run["lane"].as_str() {
                let _ = x.release_lane(Path::new(script), agent);
            }
            run["finished"] = json!(now());
            run["lane"] = Value::Null;
            run["abandoned"] = json!(true);
            save_run(&dir, &run)?;
            x.h.log(format!("closed {id}, left open by {agent}"));
        }
    }
    Ok(())
}

/// Runs that have held their lane past LANE_HOLD_MAX are closed and the lane let go (a crashed or abandoned run).
fn lane_sweep(x: &Ext) {
    let state = x.state();
    let _guard = lock(&x.runs);
    let Ok(entries) = std::fs::read_dir(state.join("runs")) else { return };
    for e in entries.flatten() {
        let Some(id) = e.file_name().to_str().map(String::from) else { continue };
        let Ok((dir, mut run)) = load_run(&state, &id) else { continue };
        let Some(script) = run["lane"].as_str().map(String::from) else { continue };
        if !run["finished"].is_null() || now().saturating_sub(run["lane_at"].as_u64().unwrap_or(0)) < LANE_HOLD_MAX {
            continue;
        }
        let agent = run["agent"].as_str().unwrap_or_default().to_string();
        let _ = x.release_lane(Path::new(&script), &agent);
        run["finished"] = json!(now());
        run["lane"] = Value::Null;
        run["abandoned"] = json!(true);
        let _ = save_run(&dir, &run);
        x.h.log(format!("closed {id}: {agent} held the lane over {} min", LANE_HOLD_MAX / 60));
    }
}

/// Runs untouched for RUN_DAYS go.
fn test_prune(state: &Path) {
    let Ok(entries) = std::fs::read_dir(state.join("runs")) else { return };
    for e in entries.flatten() {
        let old = e
            .path()
            .join("run.json")
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_secs() > RUN_DAYS * 86_400);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// A file for a still: only from this node's work folder (every job's folder, desktop's stills included) or this
/// extension's state folder. `allowed()` also lets in the node's roots, which can be a whole home folder.
fn own_folder_file(x: &Ext, f: &str) -> anyhow::Result<PathBuf> {
    let path = x.h.allowed(f)?;
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let here = canon(&path);
    let work = x.h.node().work_dir;
    let ok = [PathBuf::from(work), x.state()]
        .iter()
        .filter(|r| !r.as_os_str().is_empty())
        .any(|r| here.starts_with(canon(r)));
    if !ok {
        bail!("file {f}: must be in this node's work folder (a job's output) or this extension's own folder");
    }
    Ok(path)
}

/// browser.test_step: one step's result, with a still: the agent's browser (default) or a file from elsewhere.
fn test_step(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let id = job.params["run"].as_str().unwrap_or_default().to_string();
    let n = job.params["step"].as_u64().context("step is the step's number, from 1")? as usize;
    let ok = job.params["ok"]
        .as_bool()
        .context("ok is true or false: did the step do what its line, and its expect, say")?;
    let note = job.params["note"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(String::from);
    let want_shot = job.params["shot"].as_bool().unwrap_or(true);
    let file = job.params["file"].as_str().map(str::trim).filter(|f| !f.is_empty());
    let state = x.state();
    let _guard = lock(&x.runs);
    let (dir, mut run) = load_run(&state, &id)?;
    if !run["finished"].is_null() {
        bail!("{id} is finished: start a new run");
    }
    let total = run["steps"].as_array().map_or(0, Vec::len);
    if n == 0 || n > total {
        bail!("step {n}: this run has steps 1 to {total}");
    }
    let agent = run["agent"].as_str().unwrap_or("agent").to_string();
    std::fs::create_dir_all(&job.work)?;

    let mut shot: Option<PathBuf> = None;
    let mut shot_error: Option<String> = None;
    if let Some(f) = file {
        // a still from somewhere else, such as desktop's screenshot of a Mac app
        // only from the folders the node lets extensions read (its roots, work and state): never a person's own files
        let path = own_folder_file(x, f)?;
        let meta = std::fs::metadata(&path).with_context(|| format!("file {f}: not there on this machine"))?;
        if !meta.is_file() || meta.len() > test::STILL_MAX {
            bail!("file {f}: must be a PNG or JPEG of at most {} MB", test::STILL_MAX / 1024 / 1024);
        }
        let head = std::fs::read(&path)?;
        let mime = test::still_mime(&head).with_context(|| format!("file {f}: not a PNG or JPEG"))?;
        let to = dir.join(format!("step-{n}.{}", if mime == "image/png" { "png" } else { "jpg" }));
        std::fs::write(&to, head)?;
        shot = Some(to);
    } else if want_shot {
        let bin = x.cli()?;
        let home = x.home()?;
        let to = dir.join(format!("step-{n}.png"));
        let args = vec!["screenshot".to_string(), to.to_string_lossy().to_string()];
        match call(x, &bin, &home, &agent, &args, &job.work, Duration::from_secs(60), &|| job.cancelled()) {
            Ok((env, _)) if env["ok"] == true && to.is_file() => {
                shot = Some(to);
                // the page it was on, for the report's lead
                if run["url"].is_null() {
                    let url = call(x, &bin, &home, &agent, &["get".into(), "url".into()], &job.work, QUICK, &|| false);
                    if let Ok((u, _)) = url {
                        if let Some(u) = u["data"]["url"].as_str().filter(|u| u.starts_with("http")) {
                            run["url"] = json!(u);
                        }
                    }
                }
            }
            Ok((env, _)) => shot_error = Some(cli::summary("screenshot", &env)),
            Err(e) => shot_error = Some(format!("{e:#}")),
        }
    }
    // a re-recorded step drops a still from before when it has none now
    for old in ["png", "jpg"] {
        let p = dir.join(format!("step-{n}.{old}"));
        if shot.as_ref() != Some(&p) {
            let _ = std::fs::remove_file(p);
        }
    }
    let step = &mut run["steps"][n - 1];
    step["ok"] = json!(ok);
    step["note"] = note.map_or(Value::Null, Value::String);
    step["shot_file"] = shot.as_ref().map_or(Value::Null, |p| json!(name_of(p)));
    step["shot_error"] =
        if shot.is_some() { Value::Null } else { shot_error.clone().map_or(Value::Null, Value::String) };
    save_run(&dir, &run)?;

    if let Some(p) = &shot {
        let out = job.work.join(format!("{id}-step-{n}.{}", ext(p)));
        std::fs::copy(p, &out)?;
        job.output(&out, "image", Role::Main, json!({"file": name_of(&out)}));
    }
    let c = test::counts(&run["steps"].as_array().cloned().unwrap_or_default());
    let text = format!(
        "step {n} of {total}: {}{}{}",
        if ok { "passed" } else { "failed" },
        if shot.is_some() { ", still kept" } else { ", no still" },
        shot_error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
    );
    Ok(json!({
        "ok": true, "run": id, "step": n, "recorded": ok, "shot": shot.is_some(), "shot_error": shot_error,
        "recorded_so_far": {"passed": c.passed, "failed": c.failed, "left": c.skipped}, "text": text,
    }))
}

/// browser.test_finish: report.html with a still for each step, and the counts; the lane let go.
fn test_finish(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let id = job.params["run"].as_str().unwrap_or_default().to_string();
    let state = x.state();
    let _guard = lock(&x.runs);
    let (dir, mut run) = load_run(&state, &id)?;
    let agent = run["agent"].as_str().unwrap_or("agent").to_string();
    if run["finished"].is_null() {
        run["finished"] = json!(now());
    }
    let started = run["started"].as_u64().unwrap_or(0);
    let took = run["finished"].as_u64().unwrap_or(0).saturating_sub(started);
    // the lane goes first: the report is written from files, and needs no browser
    if let Some(script) = run["lane"].as_str().map(String::from) {
        let _ = x.release_lane(Path::new(&script), &agent);
        run["lane"] = Value::Null;
    }
    save_run(&dir, &run)?;

    let mut shown = run.clone();
    if let Some(obj) = shown.as_object_mut() {
        obj.remove("lane");
        obj.remove("abandoned");
    }
    for step in shown["steps"].as_array_mut().into_iter().flatten() {
        let file = step["shot_file"].take();
        step["shot"] = match file.as_str().map(|f| dir.join(f)) {
            Some(p) => test::data_url(&p).map(Value::String).unwrap_or(Value::Null),
            None => Value::Null,
        };
        step.as_object_mut().map(|o| o.remove("shot_file"));
    }
    let html = test::render(&shown, took);
    std::fs::create_dir_all(&job.work)?;
    let path = job.work.join("report.html");
    std::fs::write(&path, &html)?;
    job.output(&path, "document", Role::Main, json!({"file": "report.html"}));
    let steps = shown["steps"].as_array().cloned().unwrap_or_default();
    let c = test::counts(&steps);
    let text = format!(
        "{}: {} passed, {} failed, {} not run, {}",
        run["name"].as_str().unwrap_or(&id),
        c.passed,
        c.failed,
        c.skipped,
        test::duration(took)
    );
    Ok(json!({
        "ok": true, "run": id, "passed": c.passed, "failed": c.failed, "skipped": c.skipped, "steps": steps.len(),
        "report": "report.html", "bytes": html.len(), "took": took, "text": text, "machine": x.h.machine(),
    }))
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    files
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn part(id: &str, kind: &str, name: &str, state: State, needs: Vec<Action>, why: String) -> Part {
    Part {
        id: id.into(),
        kind: kind.into(),
        name: Some(name.into()),
        state: state.into(),
        needs: needs.into_iter().map(Into::into).collect(),
        why: Some(why),
        ..Default::default()
    }
}

/// The install's last error, or None while one is going.
fn lock_try(m: &Mutex<Option<String>>) -> Option<Option<String>> {
    match m.try_lock() {
        Ok(g) => Some(g.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Health's two parts: agent-browser itself, and the Chrome it drives.
fn health(x: &Ext) -> Vec<Part> {
    let state = x.state();
    let pinned = format!("agent-browser {} (pinned, sha256 checked)", install::VERSION);
    let found = std::env::var_os("POINTMAN_BROWSER_CLI").map(PathBuf::from).or_else(|| install::installed(&state));
    let Some(_bin) = found else {
        let why = match lock_try(&x.installing) {
            None => format!("installing {pinned}"),
            Some(Some(e)) => format!("couldn't install {pinned}: {e}"),
            Some(None) => format!("not installed yet: {pinned} installs at the first call"),
        };
        return vec![part("cli", "cli", "agent-browser", State::Failing, vec![Action::Update], why)];
    };
    let cli = Part {
        version: Some(install::VERSION.into()),
        ..part("cli", "cli", "agent-browser", State::Ok, vec![], pinned)
    };
    let chrome = if let Some(p) = x.system_chrome() {
        if p.is_file() {
            part("chrome", "browser", "Chrome", State::Ok, vec![], format!("system browser {}", p.display()))
        } else {
            part(
                "chrome",
                "browser",
                "Chrome",
                State::Failing,
                vec![],
                format!("the Chrome setting names {}, which isn't there", p.display()),
            )
        }
    } else if std::env::var_os("POINTMAN_BROWSER_CLI").is_some() {
        part("chrome", "browser", "Chrome", State::Ok, vec![], "stand-in".into())
    } else {
        match x.home().ok().and_then(|h| downloaded_chrome(&h)) {
            Some(v) => part(
                "chrome",
                "browser",
                "Chrome",
                State::Ok,
                vec![],
                format!("Chrome {v} (downloaded by agent-browser)"),
            ),
            None => match lock_try(&x.chrome_installing) {
                None => part("chrome", "browser", "Chrome", State::Unspecified, vec![], "downloading Chrome".into()),
                Some(Some(e)) => {
                    let hint = if cfg!(target_os = "linux") {
                        " On NixOS a downloaded Chrome can't run: set this extension's Chrome setting to a Chromium (programs.chromium or pkgs.chromium)."
                    } else {
                        ""
                    };
                    part("chrome", "browser", "Chrome", State::Failing, vec![Action::Update], format!("{e}.{hint}"))
                }
                Some(None) => part(
                    "chrome",
                    "browser",
                    "Chrome",
                    State::Unspecified,
                    vec![Action::Update],
                    "Chrome downloads at the first call".into(),
                ),
            },
        }
    };
    vec![cli, chrome]
}

fn fix(x: &Ext, req: &HealthFixRequest) -> anyhow::Result<HealthFixResponse> {
    let message = match (req.part.as_str(), req.action.as_known()) {
        ("cli", Some(Action::Update)) => {
            let p = x.cli()?;
            format!("agent-browser {} is at {}.", install::VERSION, p.display())
        }
        ("chrome", Some(Action::Update)) => {
            x.ensure_chrome()?;
            "Chrome is installed for agent-browser.".to_string()
        }
        _ => "Nothing to fix from here.".to_string(),
    };
    Ok(HealthFixResponse { message, ..Default::default() })
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let x = Arc::new(Ext {
        h: ext.handle(),
        installing: Mutex::new(None),
        chrome_installing: Mutex::new(None),
        runs: Mutex::new(()),
        counter: AtomicU64::new(0),
    });

    // the pinned CLI and its Chrome are fetched as soon as the extension starts, so the first call doesn't wait
    let xs = x.clone();
    ext.on_start(move |_: &Handle| {
        let xs_sweep = xs.clone();
        let xs = xs.clone();
        std::thread::spawn(move || {
            if let Err(e) = xs.cli().and_then(|_| xs.ensure_chrome()) {
                xs.h.log(format!("{e:#}"));
            }
        });
        let xl = xs_sweep.clone();
        std::thread::spawn(move || loop {
            lane_sweep(&xl);
            std::thread::sleep(Duration::from_secs(60));
        });
    });

    type Cmd = fn(&Ext, &Job) -> anyhow::Result<Value>;
    let commands: [(&str, Cmd); 5] = [
        ("browser.run", run),
        ("browser.status", status),
        ("browser.test_start", test_start),
        ("browser.test_step", test_step),
        ("browser.test_finish", test_finish),
    ];
    for (kind, f) in commands {
        let xc = x.clone();
        ext.command(kind, move |job: &Job| f(&xc, job));
    }

    let xh = x.clone();
    ext.on(move |_: HealthCheckRequest| Ok(HealthCheckResponse { parts: health(&xh), ..Default::default() }));
    let xf = x.clone();
    ext.on(move |req: HealthFixRequest| fix(&xf, &req));

    ext.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overflow_file_stays_in_the_jobs_folder() {
        assert_eq!(file_stem("snapshot"), "snapshot");
        assert_eq!(file_stem("../../.ssh/x"), "sshx");
        assert_eq!(file_stem(""), "answer");
    }

    #[test]
    fn a_job_text_never_says_window_or_keys_smoke() {
        assert_eq!(lane_job("Staging: sign in"), "UI test: Staging: sign in");
        let t = lane_job("Window resize + keys-smoke WINDOW");
        assert!(!t.to_lowercase().contains("window") && !t.to_lowercase().contains("keys-smoke"), "{t}");
    }

    #[test]
    fn a_downloaded_chrome_is_found_by_its_folder() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(downloaded_chrome(home.path()), None);
        std::fs::create_dir_all(home.path().join(".agent-browser/browsers/chrome-157.0.8103.38")).unwrap();
        assert_eq!(downloaded_chrome(home.path()).as_deref(), Some("157.0.8103.38"));
    }
}

#[cfg(test)]
mod nix_tests {
    use super::*;

    #[test]
    fn the_nixos_chromium_is_the_out_link_in_the_state_folder() {
        let state = Path::new("/state/browser");
        assert_eq!(nix_chrome_link(state), Path::new("/state/browser/chromium"));
        assert_eq!(nix_chrome_bin(state), Path::new("/state/browser/chromium/bin/chromium"));
    }

    #[test]
    fn nixos_is_decided_by_etc_nixos_unless_overridden() {
        // one test, since it sets the process's environment
        std::env::set_var("POINTMAN_BROWSER_NIXOS", "1");
        assert!(is_nixos());
        std::env::set_var("POINTMAN_BROWSER_NIXOS", "0");
        assert!(!is_nixos());
        std::env::remove_var("POINTMAN_BROWSER_NIXOS");
        assert_eq!(is_nixos(), Path::new("/etc/NIXOS").exists());
    }
}
