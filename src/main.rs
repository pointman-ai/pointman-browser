//! The extension's process: the node starts it (`bin/browser`, from the extension's folder) and talks to it over
//! stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! `browser.run` runs one agent-browser command on this machine for an agent anywhere (create_job with `machine`),
//! in a session named for that agent: its own tabs, cookies and refs, in a clean headless Chrome that's never a
//! person's browser. Each call runs on a thread of its own and can be cancelled: the CLI is killed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use browser::cli::{self, Call};
use browser::login::{self, How, Login};
use browser::{install, signin, test, vars};
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::pane_open_response::{video, Video};
use pointman_extension::protocol::{
    credentials_sign_in_response, CredentialsCodeRequest, CredentialsCodeResponse, CredentialsSignInRequest,
    CredentialsSignInResponse, HealthCheckRequest, HealthCheckResponse, HealthFixRequest, HealthFixResponse,
    PaneControlRequest, PaneOpenResponse,
};
use pointman_extension::v1::health::part::State;
use pointman_extension::v1::health::{Action, Part};
use pointman_extension::v1::PaneInput;
use pointman_extension::{convert, Extension, Handle, Job, Pane, Refused};
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
    /// The sign-ins people are doing themselves, by core's sign-in id.
    signins: Mutex<HashMap<String, SignIn>>,
    /// The logins each agent's session has used, by label, in memory only: a hidden value typed into a page stays
    /// there, so every later answer from that session is scrubbed of it too, with or without `login`.
    used: Mutex<HashMap<String, HashMap<String, Login>>>,
}

impl Ext {
    /// Every hidden value `agent`'s session has had, scrubbed from `answer`.
    fn scrub(&self, agent: &str, answer: &mut Value) {
        for l in lock(&self.used).get(agent).into_iter().flat_map(|m| m.values()) {
            l.scrub(answer);
        }
    }

    fn scrub_text(&self, agent: &str, text: &str) -> String {
        let mut v = Value::String(text.to_string());
        self.scrub(agent, &mut v);
        v.as_str().unwrap_or_default().to_string()
    }

    /// `agent`'s session has `l` now.
    fn remember(&self, agent: &str, label: &str, l: &Login) {
        lock(&self.used).entry(agent.to_string()).or_default().insert(label.to_string(), l.clone());
    }
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

    /// A text setting that isn't empty.
    fn setting_set(&self, name: &str) -> bool {
        self.h.settings()[name].as_str().is_some_and(|p| !p.trim().is_empty())
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

    /// The Chrome to use: the `chrome` setting; on NixOS the extension's own out-link; on other Linux one on the PATH.
    fn system_chrome(&self) -> Option<PathBuf> {
        if let Some(p) = self.h.settings()["chrome"].as_str().filter(|p| !p.trim().is_empty()) {
            return Some(PathBuf::from(p.trim()));
        }
        if is_nixos() {
            // on NixOS: the setting or this extension's own out-link, never whatever is on the PATH
            return self.nix_chrome();
        }
        if !cfg!(target_os = "linux") {
            return None;
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
        let set = self.setting_set("chrome");
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
        let argv = nix_build_argv(&link);
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
        self.ensure_nix_chrome()?;
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

/// The nixpkgs revision Chromium is built from on NixOS: nixos-unstable's Hydra-built head on 2026-11-07
/// (channels.nixos.org/nixos-unstable/git-revision), so cache.nixos.org has it and every machine gets the same one.
const NIXPKGS_REV: &str = "f4a03c008967841f2a0b3cb7c436fc6fc31671f6";

/// The words after `nix` that build Chromium into `link`.
fn nix_build_argv(link: &Path) -> Vec<String> {
    [
        "build",
        &format!("github:NixOS/nixpkgs/{NIXPKGS_REV}#chromium"),
        "--out-link",
        &link.to_string_lossy(),
        "--extra-experimental-features",
        "nix-command flakes",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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

/// The test login `label` (the vault's `browser.<label>`), read and checked. Errors name the label and keys, never a
/// value.
fn load_login(x: &Ext, label: &str) -> anyhow::Result<Login> {
    let item = login::item_name(label)?;
    let text =
        x.h.secret(&item)
            .map_err(|_| anyhow!("there is no login named {label}: James adds one with `/browser add {label}`"))?;
    Login::parse(&text).map_err(|e| anyhow!("the login {label} isn't usable: {e}"))
}

/// Sign `agent`'s session in with a test login, without the agent seeing a value, in MB-926's order: a saved session
/// (STATE) is loaded from a private file that's gone straight after; else URL + EMAIL or USERNAME + PASSWORD sign in
/// with agent-browser's `auth`, the password on its stdin, never its argv, and the saved profile deleted again
/// whatever happens; else the URL is opened. The Cloudflare Access headers go to that one origin (agent-browser scopes
/// `--headers` to it), in a private config file, never on a command line.
fn sign_in(
    x: &Ext,
    bin: &Path,
    home: &Path,
    agent: &str,
    l: &Login,
    cwd: &Path,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    let how = l.how();
    if how == How::None {
        return Ok(());
    }
    let profile = format!("pb-{agent}");
    let step = |args: Vec<String>, input: Option<&[u8]>, what: &str| -> anyhow::Result<()> {
        let mut argv = vec!["--session".to_string(), agent.to_string(), "--json".to_string()];
        argv.extend(args);
        let vars = x.env(home)?;
        let c = Call { bin, argv: &argv, env: &vars, cwd, timeout: Duration::from_secs(90) };
        let ran = cli::run_with_input(&c, input, cancelled)?;
        if ran.cancelled {
            bail!(pointman_extension::Cancelled);
        }
        let env = cli::envelope(&ran).map_err(|e| anyhow!("{what}: {}", l.scrub_text(&format!("{e:#}"))))?;
        if env["success"].as_bool() == Some(false) || env["ok"].as_bool() == Some(false) {
            let why = env["error"]["message"].as_str().or(env["error"].as_str()).unwrap_or("it failed");
            bail!("{what} failed: {}", l.scrub_text(why));
        }
        Ok(())
    };
    let s = |v: &str| v.to_string();
    let tag = format!("{}-{}", std::process::id(), x.counter.fetch_add(1, Ordering::SeqCst) + 1);
    // a site behind Cloudflare Access: the token goes in a private config file (agent-browser's `headers` key), never
    // on a command line where `ps` shows it; the file is gone as soon as the sign-in's opening calls return
    let config = cwd.join(format!("login-{tag}.json"));
    let with_access = |mut args: Vec<String>| -> Vec<String> {
        if l.access.is_some() {
            args.splice(0..0, [s("--config"), config.to_string_lossy().to_string()]);
        }
        args
    };
    if let Some(headers) = l.access_headers() {
        write_private(&config, &json!({"headers": headers}).to_string())?;
    }
    let result = (|| match how {
        How::State => {
            // the saved session: cookies and storage, from a file only this user reads, deleted straight after
            let file = cwd.join(format!("state-{tag}.json"));
            write_private(&file, l.state().unwrap_or_default())?;
            let loaded = step(
                with_access(vec![s("state"), s("load"), file.to_string_lossy().to_string()]),
                None,
                "loading the saved session",
            );
            let _ = std::fs::remove_file(&file);
            loaded?;
            match l.url() {
                Some(url) => step(with_access(vec![s("open"), url.to_string()]), None, "opening the site"),
                None => Ok(()),
            }
        }
        How::Password => {
            let url = l.url().unwrap_or_default().to_string();
            step(with_access(vec![s("open"), url.clone()]), None, "opening the sign-in page")?;
            let saved = step(
                vec![
                    s("auth"),
                    s("save"),
                    profile.clone(),
                    s("--url"),
                    url,
                    s("--username"),
                    l.user().unwrap_or_default().to_string(),
                    s("--password-stdin"),
                ],
                Some(l.password().unwrap_or_default().as_bytes()),
                "saving the login",
            )
            .and_then(|_| step(vec![s("auth"), s("login"), profile.clone()], None, "signing in"));
            let _ = step(vec![s("auth"), s("delete"), profile.clone()], None, "removing the saved login");
            saved
        }
        How::Open => {
            step(with_access(vec![s("open"), l.url().unwrap_or_default().to_string()]), None, "opening the site")
        }
        How::None => Ok(()),
    })();
    let _ = std::fs::remove_file(&config);
    result
}

/// A file only this user can read, made with that mode from the start.
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path).and_then(|mut f| f.write_all(text.as_bytes())).with_context(|| format!("writing {}", path.display()))
}

/// The `login` param: a label, or none.
fn login_label(job: &Job) -> anyhow::Result<Option<String>> {
    match &job.params["login"] {
        Value::Null => Ok(None),
        Value::String(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        _ => bail!("login is the label of a test login James added with `/browser add`, such as stagesheep"),
    }
}

/// The `agent` param as its session's name: the agent's handle, never a sign-in's session.
fn agent_session(job: &Job, what: &str) -> anyhow::Result<String> {
    let agent = cli::session_name(job.params["agent"].as_str())
        .with_context(|| format!("agent is your handle, with letters or digits: {what}"))?;
    if signin::is_signin_session(&agent) {
        bail!("agent is your handle: sessions named {}… are the sign-in pane's, never an agent's", signin::PREFIX);
    }
    Ok(agent)
}

/// browser.run: one agent-browser command in the agent's own session, its envelope back, a screenshot posted. With a
/// login, the session is signed in first and `{{NAME}}` in args is that login's value (src/vars.rs).
fn run(x: &Ext, job: &Job) -> anyhow::Result<Value> {
    let args = strings(&job.params["args"], "args")?;
    if args.is_empty() {
        bail!("args is empty: the words after `agent-browser`, [\"open\", \"https://example.com\"]");
    }
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    let allowed = |p: &Path| x.h.allowed(p);
    if let Some(why) = cli::refused(&args, &job.work, &allowed) {
        bail!(why);
    }
    if cli::wants_headed(&args) && !x.setting("headed", false) {
        bail!(
            "A visible browser window is off on {} (the Browser extension's \"Visible window\" setting): browse \
             headless, or ask this machine's person to turn it on.",
            x.h.machine()
        );
    }
    let agent = agent_session(job, "each agent has its own browser session, named for it")?;
    let timeout = Duration::from_secs(job.params["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT).clamp(1, MAX_TIMEOUT));

    let label = login_label(job)?;
    let signed = label.as_deref().map(|l| load_login(x, l)).transpose()?;
    // the login's values go in now, so a name it hasn't got, or a hidden value out of place, stops the run before
    // anything reaches the browser
    let (args, hidden) = vars::fill_in(&args, label.as_deref().zip(signed.as_ref()))?;
    if let Some(l) = &signed {
        if let Some(why) = cli::refused(&args, &job.work, &allowed) {
            bail!("with the login's values put in: {}", l.scrub_text(&why));
        }
    }
    if let (Some(label), Some(l)) = (&label, &signed) {
        x.remember(&agent, label, l);
    }
    let scrubbed = |e: anyhow::Error| anyhow!("{}", x.scrub_text(&agent, &format!("{e:#}")));
    job.progress(None, "agent-browser");
    let bin = x.cli()?;
    x.ensure_chrome()?;
    let home = x.home()?;
    std::fs::create_dir_all(&job.work).with_context(|| format!("making {}", job.work.display()))?;
    if let Some(l) = &signed {
        job.progress(None, "signing in");
        sign_in(x, &bin, &home, &agent, l, &job.work, &|| job.cancelled())?;
    }
    if hidden {
        // a hidden value is typed only on the login's own site
        let origin = signed.as_ref().and_then(|l| l.origin.clone()).unwrap_or_default();
        let get = ["get".to_string(), "url".to_string()];
        let (page, _) =
            call(x, &bin, &home, &agent, &get, &job.work, QUICK, false, &|| job.cancelled()).map_err(scrubbed)?;
        let url = page["data"]["url"].as_str().unwrap_or_default();
        if !login::same_origin(url, &origin) {
            let on = login::origin_of(url).unwrap_or_else(|_| "no https page".into());
            bail!("a hidden value is typed only on {origin}, and the page is on {on}: open the login's site first");
        }
    }
    let before = files_in(&job.work);

    let (mut env, command) =
        call(x, &bin, &home, &agent, &args, &job.work, timeout, hidden, &|| job.cancelled()).map_err(scrubbed)?;
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
    // agent-browser echoes typed text, and a page keeps what was typed: nothing hidden this session has had leaves in
    // an answer, or its overflow file
    x.scrub(&agent, &mut answer);
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
    x.scrub(&agent, &mut answer);
    answer["session"] = Value::String(agent);
    answer["machine"] = Value::String(x.h.machine());
    if let Some(l) = &signed {
        answer["login"] = Value::String(label.clone().unwrap_or_default());
        answer["vars"] = l.vars_answer();
        answer["signed_in"] = json!(l.how().word());
    }
    Ok(answer)
}

/// One call in `agent`'s session: `agent-browser --session <agent> --json <args>`. Returns its envelope and the
/// command's name (the first word that isn't a flag). `on_stdin`: the command goes on agent-browser's stdin as a
/// one-command `batch`, so a hidden value in it is in no argv `ps` shows.
#[allow(clippy::too_many_arguments)]
fn call(
    x: &Ext,
    bin: &Path,
    home: &Path,
    agent: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    on_stdin: bool,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<(Value, String)> {
    let mut argv = vec!["--session".to_string(), agent.to_string(), "--json".to_string()];
    let input = if on_stdin {
        argv.extend(["batch", "--json", "--omit-command"].map(String::from));
        Some(serde_json::to_vec(&[args])?)
    } else {
        argv.extend(args.iter().cloned());
        None
    };
    let vars = x.env(home)?;
    let c = Call { bin, argv: &argv, env: &vars, cwd, timeout };
    let ran = cli::run_with_input(&c, input.as_deref(), cancelled)?;
    if ran.cancelled {
        bail!(pointman_extension::Cancelled);
    }
    let env = if on_stdin { cli::batch_envelope(&ran)? } else { cli::envelope(&ran)? };
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

/// browser.add: the apps' form for a test login seals its value into the vault, so there is nothing for the
/// extension to do; this is the command the form hangs from.
fn add(_x: &Ext, _job: &Job) -> anyhow::Result<Value> {
    Ok(json!({"ok": true, "text": "Add a test login with /browser add <label> in the app: name its values (URL, \
        EMAIL, PASSWORD…), each shown or hidden, or sign in yourself in the browser pane. Agents use it as login, and \
        its values as {{NAME}}."}))
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
        // NixOS never uses a downloaded Chrome: until the first call builds Chromium, say so
        None if is_nixos() => json!({"kind": "not_built"}),
        None => match downloaded_chrome(&home) {
            Some(v) => json!({"kind": "downloaded", "version": v}),
            None => json!({"kind": "none"}),
        },
    };
    let version =
        if std::env::var_os("POINTMAN_BROWSER_CLI").is_some() { version_of(&bin) } else { install::VERSION.into() };
    let text = format!(
        "agent-browser {version} on {}: {}, {} open session{}",
        x.h.machine(),
        chrome_text(&chrome),
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

/// What `browser.status` says about the Chrome, from its `chrome` answer.
fn chrome_text(chrome: &Value) -> String {
    match chrome["kind"].as_str() {
        Some("system") => format!("Chrome (system) {}", chrome["path"].as_str().unwrap_or("")),
        Some("downloaded") => format!("Chrome {}", chrome["version"].as_str().unwrap_or("?")),
        Some("not_built") => "Chromium not built yet (builds on the first call)".into(),
        _ => "Chrome not installed yet".into(),
    }
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
    let agent = agent_session(job, "the run uses your own browser session")?;
    let label = login_label(job)?;
    let signed = label.as_deref().map(|l| load_login(x, l)).transpose()?;
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
    if let (Some(label), Some(l)) = (&label, &signed) {
        job.progress(None, "signing in");
        let done = x.cli().and_then(|bin| {
            x.ensure_chrome()?;
            let home = x.home()?;
            std::fs::create_dir_all(&job.work)?;
            sign_in(x, &bin, &home, &agent, l, &job.work, &|| job.cancelled())
        });
        x.remember(&agent, label, l);
        match done {
            Ok(_) => run["login"] = json!(label),
            Err(e) => {
                if let Some(script) = x.lane() {
                    let _ = x.release_lane(&script, &agent);
                }
                return Err(e);
            }
        }
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
    let mut answer = json!({"ok": true, "run": id, "steps": steps.iter().map(|s| json!({"text": s.text, "expect": s.expect})).collect::<Vec<_>>(), "text": text, "machine": x.h.machine()});
    if let (Some(label), Some(l)) = (&label, &signed) {
        answer["login"] = json!(label);
        answer["vars"] = l.vars_answer();
        answer["signed_in"] = json!(l.how().word());
    }
    Ok(answer)
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
        match call(x, &bin, &home, &agent, &args, &job.work, Duration::from_secs(60), false, &|| job.cancelled()) {
            Ok((env, _)) if env["ok"] == true && to.is_file() => {
                shot = Some(to);
                // the page it was on, for the report's lead
                if run["url"].is_null() {
                    let url =
                        call(x, &bin, &home, &agent, &["get".into(), "url".into()], &job.work, QUICK, false, &|| false);
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

// ------------------------------------------------------------------ sign in yourself (EXT-156)

/// A sign-in the person does themselves in a session of the extension's own (`signin-<id>`, never an agent's), shown
/// in the `signin` pane. It ends with Save, the pane's close, or 15 minutes idle, and its files go with it.
struct SignIn {
    session: String,
    /// The sign-in page's host, for the pane's title.
    site: String,
    /// Who started it: only they see its pane.
    person: (String, String),
    /// Its folder: `<state>/signins/<session>`, the saved state while Save runs.
    dir: PathBuf,
    /// agent-browser's stream for the session, on 127.0.0.1.
    port: u16,
    last: u64,
    control: signin::Control,
    stream: Option<Arc<Stream>>,
    pane: Option<Pane>,
}

/// The session's stream (agent-browser's WebSocket): frames come in, input goes out.
struct Stream {
    ws: Mutex<tungstenite::WebSocket<std::net::TcpStream>>,
    stop: AtomicBool,
    /// The page's size in CSS pixels, from the last frame: where 0-1 of the picture lands.
    size: Mutex<(f64, f64)>,
}

impl Stream {
    fn connect(port: u16, fps: u32) -> anyhow::Result<Stream> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let tcp = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .with_context(|| format!("the sign-in's stream on port {port}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (ws, _) = tungstenite::client::client(format!("ws://127.0.0.1:{port}/?maxFps={fps}"), tcp)
            .map_err(|e| anyhow!("the sign-in's stream: {e}"))?;
        // short reads from here: input waits at most this long for the reader to let go
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(40)))?;
        Ok(Stream { ws: Mutex::new(ws), stop: AtomicBool::new(false), size: Mutex::new(signin::VIEWPORT) })
    }

    fn send(&self, message: &Value) -> anyhow::Result<()> {
        lock(&self.ws).send(tungstenite::Message::text(message.to_string())).map_err(|e| anyhow!("the stream: {e}"))
    }

    fn live(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
    }
}

/// Frames from the stream to the pane, while someone watches, until it stops.
fn pump(stream: Arc<Stream>, pane: Pane) {
    use base64::Engine as _;
    while stream.live() && pane.closed().is_none() {
        let read = lock(&stream.ws).read();
        match read {
            Ok(tungstenite::Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if v["type"] != "frame" {
                    continue;
                }
                let m = &v["metadata"];
                let (Some(w), Some(h)) = (m["deviceWidth"].as_f64(), m["deviceHeight"].as_f64()) else { continue };
                if w > 0.0 && h > 0.0 {
                    *lock(&stream.size) = (w, h);
                }
                let Some(data) = v["data"].as_str() else { continue };
                let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(data) else { continue };
                let Some((pw, ph)) = signin::jpeg_size(&jpeg) else { continue };
                if pane.viewers() > 0 {
                    let at = m["timestamp"].as_f64().map_or_else(|| now() * 1000, |t| t as u64);
                    pane.frame(&jpeg, pw, ph, at);
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                pane.handle().log(format!("pane {}: the sign-in's stream ended: {e}", pane.id()));
                break;
            }
        }
    }
    stream.stop.store(true, Ordering::SeqCst);
}

/// The sign-in session's environment: the extension's, with the stream's frames sized for the pane, and the
/// browser's own idle stop a little after the extension's.
fn signin_env(x: &Ext, home: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let mut env = x.env(home)?;
    for (k, v) in [
        ("AGENT_BROWSER_STREAM_QUALITY", "70"),
        ("AGENT_BROWSER_STREAM_MAX_WIDTH", "1280"),
        ("AGENT_BROWSER_STREAM_MAX_HEIGHT", "720"),
        ("AGENT_BROWSER_IDLE_TIMEOUT_MS", "1200000"),
    ] {
        env.push((k.to_string(), v.to_string()));
    }
    Ok(env)
}

/// One call in a sign-in's session, its envelope back; `ok: false` is an error with agent-browser's message.
fn signin_call(x: &Ext, session: &str, args: &[&str], timeout: Duration) -> anyhow::Result<Value> {
    let bin = x.cli()?;
    let home = x.home()?;
    let mut argv = vec!["--session".to_string(), session.to_string(), "--json".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let env = signin_env(x, &home)?;
    let c = Call { bin: &bin, argv: &argv, env: &env, cwd: &home, timeout };
    let got = cli::envelope(&cli::run(&c, &|| false)?)?;
    if got["ok"] != true {
        bail!("{}", cli::summary(args.first().copied().unwrap_or("agent-browser"), &got));
    }
    Ok(got)
}

/// The same person: by id when both have one, else by name.
fn same_person(a: &(String, String), b: &(String, String)) -> bool {
    if !a.0.is_empty() && !b.0.is_empty() {
        a.0 == b.0
    } else {
        !a.1.is_empty() && a.1 == b.1
    }
}

fn the_form(form: &str) -> anyhow::Result<()> {
    match form {
        "login" => Ok(()),
        other => bail!("browser has one sign-in form, login, not {}", if other.is_empty() { "none" } else { other }),
    }
}

/// `credentials.sign_in` with `start`: a session of its own opens the sign-in page, and the answer is the pane to show
/// it in, with what to tell the person.
fn credentials_sign_in(x: &Ext, req: &CredentialsSignInRequest) -> anyhow::Result<CredentialsSignInResponse> {
    the_form(&req.form)?;
    login::item_name(&req.label)?;
    // only the person it's for may watch and drive it: a sign-in with no person is refused, never open to anyone
    let person = req.person.as_option().map(|p| (p.id.clone(), p.name.clone())).unwrap_or_default();
    if person.0.is_empty() && person.1.is_empty() {
        bail!("a sign-in is for one person, and this one came with none");
    }
    let id = req.session.as_option().map(|s| s.id.clone()).unwrap_or_default();
    let session = signin::session_name(&id)?;
    let start = req.start.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let start = start.context("Sign in yourself starts at the site's sign-in page: give its https link")?;
    if start.len() > 2048 {
        bail!("the sign-in page's link is longer than 2048 characters");
    }
    let site = login::origin_of(start).map_err(|_| anyhow!("the sign-in page must be an https link"))?;
    let site = site.trim_start_matches("https://").to_string();
    let pane = credentials_sign_in_response::Pane {
        open: "signin".into(),
        target: convert::to_struct(&json!({"session": id})).into(),
        ..Default::default()
    };
    let answer =
        CredentialsSignInResponse { pane: pane.into(), prompt: Some(signin::prompt(start)), ..Default::default() };
    {
        let mut open = lock(&x.signins);
        if let Some(s) = open.get_mut(&id) {
            // the same sign-in asked again: the same pane
            s.last = now();
            return Ok(answer);
        }
        if open.len() >= signin::OPEN_MAX {
            bail!("{} sign-ins are open on {} already: finish or cancel one first", open.len(), x.h.machine());
        }
    }
    x.cli()?;
    x.ensure_chrome()?;
    let dir = x.state().join("signins").join(&session);
    private_dir(&dir)?;
    std::fs::write(dir.join("session"), &session)?;
    let started = (|| {
        signin_call(x, &session, &["open", start], Duration::from_secs(90))?;
        let mut status = signin_call(x, &session, &["stream", "status"], QUICK)?;
        if status["data"]["enabled"] != true {
            signin_call(x, &session, &["stream", "enable"], QUICK)?;
            status = signin_call(x, &session, &["stream", "status"], QUICK)?;
        }
        status["data"]["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .context("agent-browser gave the sign-in no stream port")
    })();
    let port = match started {
        Ok(p) => p,
        Err(e) => {
            let _ = signin_call(x, &session, &["close"], QUICK);
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.context("starting the sign-in"));
        }
    };
    let s = SignIn {
        session: session.clone(),
        site,
        person,
        dir,
        port,
        last: now(),
        control: signin::Control::default(),
        stream: None,
        pane: None,
    };
    lock(&x.signins).insert(id, s);
    x.h.log(format!("{session}: started"));
    Ok(answer)
}

/// `credentials.code` "save": the session's cookies and storage, with the origin it's on, as the login's record. The
/// sign-in ends after; a page that isn't on an https site yet keeps it open, to sign in and Save again.
fn credentials_code(x: &Ext, req: &CredentialsCodeRequest) -> anyhow::Result<CredentialsCodeResponse> {
    the_form(&req.form)?;
    let id = req.session.as_option().map(|s| s.id.clone()).unwrap_or_default();
    if req.code != "save" {
        bail!("this sign-in is kept with Save, beside its pane");
    }
    let (session, dir) = {
        let mut open = lock(&x.signins);
        let s = open
            .get_mut(&id)
            .context("this sign-in has ended (cancelled, closed, or 15 minutes idle): start it again")?;
        s.last = now();
        (s.session.clone(), s.dir.clone())
    };
    let page = signin_call(x, &session, &["get", "url"], QUICK)?;
    let url = page["data"]["url"].as_str().unwrap_or_default();
    let origin =
        login::origin_of(url).map_err(|_| anyhow!("the pane isn't on an https site yet: sign in, then Save"))?;
    let file = dir.join("state.json");
    let path = file.to_string_lossy().to_string();
    let saved = signin_call(x, &session, &["state", "save", path.as_str()], QUICK)
        .and_then(|_| std::fs::read_to_string(&file).context("agent-browser saved no state"));
    let _ = std::fs::remove_file(&file);
    let (value, note) = signin::record(&origin, &saved?)?;
    signin_end(x, &id, "saved", true);
    if let Some(note) = &note {
        x.h.log(format!("{session}: {note}"));
    }
    Ok(CredentialsCodeResponse { value, note, ..Default::default() })
}

/// The sign-in is over: its stream, its pane (unless the pane's close is why), its session and its folder go.
fn signin_end(x: &Ext, id: &str, reason: &str, close_pane: bool) {
    let Some(s) = lock(&x.signins).remove(id) else { return };
    if let Some(stream) = &s.stream {
        stream.stop.store(true, Ordering::SeqCst);
    }
    if close_pane {
        if let Some(p) = &s.pane {
            p.close(reason);
        }
    }
    let _ = signin_call(x, &s.session, &["close"], QUICK);
    let _ = std::fs::remove_dir_all(&s.dir);
    x.h.log(format!("{}: ended ({reason})", s.session));
}

/// How long a sign-in may sit untouched (POINTMAN_BROWSER_SIGNIN_IDLE_SECS shortens it, for the tests).
fn idle_secs() -> u64 {
    std::env::var("POINTMAN_BROWSER_SIGNIN_IDLE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(signin::IDLE_SECS)
}

/// How often the sweeper looks (POINTMAN_BROWSER_SWEEP_SECS, for the tests).
fn sweep_secs() -> u64 {
    std::env::var("POINTMAN_BROWSER_SWEEP_SECS").ok().and_then(|s| s.parse().ok()).filter(|s| *s > 0).unwrap_or(60)
}

/// Sign-ins untouched for 15 minutes end.
fn signin_sweep(x: &Ext) {
    let idle: Vec<String> = lock(&x.signins)
        .iter()
        .filter(|(_, s)| now().saturating_sub(s.last) >= idle_secs())
        .map(|(id, _)| id.clone())
        .collect();
    for id in idle {
        signin_end(x, &id, "15 minutes idle", true);
    }
}

/// Sign-ins a previous run of the extension left (it stopped, or the machine did): their sessions and folders go.
fn signin_leftovers(x: &Ext) {
    let Ok(entries) = std::fs::read_dir(x.state().join("signins")) else { return };
    for e in entries.flatten() {
        if let Ok(session) = std::fs::read_to_string(e.path().join("session")) {
            if signin::is_signin_session(session.trim()) {
                let _ = signin_call(x, session.trim(), &["close"], QUICK);
            }
        }
        let _ = std::fs::remove_dir_all(e.path());
    }
}

/// The sign-in's stream, connected now if it isn't, with its frames going to `pane`.
fn signin_stream(x: &Ext, id: &str) -> anyhow::Result<Arc<Stream>> {
    let (port, pane) = {
        let open = lock(&x.signins);
        let s = open.get(id).context("the sign-in has ended")?;
        if let Some(st) = s.stream.as_ref().filter(|st| st.live()) {
            return Ok(st.clone());
        }
        (s.port, s.pane.clone().context("the sign-in's pane isn't open")?)
    };
    let stream = Arc::new(Stream::connect(port, 10)?);
    let mut open = lock(&x.signins);
    let s = open.get_mut(id).context("the sign-in has ended")?;
    if let Some(st) = s.stream.as_ref().filter(|st| st.live()) {
        stream.stop.store(true, Ordering::SeqCst);
        return Ok(st.clone());
    }
    s.stream = Some(stream.clone());
    let feed = stream.clone();
    std::thread::spawn(move || pump(feed, pane));
    Ok(stream)
}

/// The stream stops when nobody watches and nobody controls it: no frames are made for no one.
fn signin_quiet(x: &Ext, id: &str, viewers: u64) {
    let mut open = lock(&x.signins);
    let Some(s) = open.get_mut(id) else { return };
    if viewers == 0 && !s.control.granted(now()) {
        if let Some(st) = s.stream.take() {
            st.stop.store(true, Ordering::SeqCst);
        }
    }
}

fn send_all(stream: &Stream, messages: &[Value]) -> anyhow::Result<()> {
    messages.iter().try_for_each(|m| stream.send(m))
}

/// The `signin` pane: the sign-in's page as mjpeg frames, while someone watches; the person's pointer, scroll, keys
/// and paste go to its page while they hold Control.
fn signin_open(x: &Arc<Ext>, pane: &Pane) -> anyhow::Result<PaneOpenResponse> {
    let id = pane.target()["session"].as_str().unwrap_or_default().to_string();
    let viewer = pane.viewer().person.as_option().map(|p| (p.id.clone(), p.name.clone())).unwrap_or_default();
    {
        let mut open = lock(&x.signins);
        let s = open.get_mut(&id).ok_or_else(|| {
            Refused::new(
                "no such sign-in on this machine: it ended, or never started here. Start it again from the form",
            )
        })?;
        if !same_person(&s.person, &viewer) {
            return Err(Refused::new("this sign-in is someone else's").into());
        }
        if let Some(old) = s.pane.replace(pane.clone()) {
            old.close("opened again");
        }
        if let Some(st) = s.stream.take() {
            st.stop.store(true, Ordering::SeqCst);
        }
        s.last = now();
    }
    let (xv, iv) = (x.clone(), id.clone());
    pane.on_viewers(move |_: &Pane, attached: u64| -> anyhow::Result<()> {
        if let Some(s) = lock(&xv.signins).get_mut(&iv) {
            s.last = now();
        }
        if attached > 0 {
            signin_stream(&xv, &iv)?;
        } else {
            signin_quiet(&xv, &iv, 0);
        }
        Ok(())
    });
    let (xc, ic) = (x.clone(), id.clone());
    pane.on_control(move |pane: &Pane, grant: Option<&PaneControlRequest>| -> anyhow::Result<()> {
        let until = match grant {
            Some(g) => Some(
                g.until.as_option().and_then(|t| u64::try_from(t.seconds).ok()).context("a grant's until is a time")?,
            ),
            None => None,
        };
        let (let_go, stream) = {
            let mut open = lock(&xc.signins);
            let s = open.get_mut(&ic).ok_or_else(|| Refused::new("the sign-in has ended"))?;
            s.last = now();
            (s.control.set(until), s.stream.clone())
        };
        if let (Some(st), false) = (stream, let_go.is_empty()) {
            let _ = send_all(&st, &let_go);
        }
        if until.is_none() {
            signin_quiet(&xc, &ic, pane.viewers());
        }
        Ok(())
    });
    let (xi, ii) = (x.clone(), id.clone());
    pane.on_input(move |_: &Pane, input: &PaneInput| -> anyhow::Result<()> {
        let mut event = input.json();
        event["kind"] = json!(input.kind);
        if input.kind == "hint" {
            // the app's own cap on the rate, as for the screen pane
            if let Some(fps) = event["max_fps"].as_f64().filter(|f| *f >= 0.0) {
                let stream = lock(&xi.signins).get(&ii).and_then(|s| s.stream.clone());
                if let Some(st) = stream {
                    st.send(&json!({"type": "config", "maxFps": fps.ceil().min(120.0) as u64}))?;
                }
            }
            return Ok(());
        }
        if !matches!(input.kind.as_str(), "pointer" | "scroll" | "key" | "text") {
            return Err(
                Refused::new("the sign-in pane takes pointer, scroll, key and text while you control it").into()
            );
        }
        // the grant first: no input without one connects anything
        let messages = {
            let mut open = lock(&xi.signins);
            let s = open.get_mut(&ii).ok_or_else(|| Refused::new("the sign-in has ended"))?;
            s.last = now();
            let size = s.stream.as_ref().map_or(signin::VIEWPORT, |st| *lock(&st.size));
            s.control
                .messages(&event, size, now(), cfg!(target_os = "macos"))
                .map_err(|e| Refused::new(e.to_string()))?
        };
        let stream = signin_stream(&xi, &ii).map_err(|e| Refused::new(format!("{e:#}")))?;
        send_all(&stream, &messages)
    });
    let (xx, ix, pid) = (x.clone(), id.clone(), pane.id().to_string());
    pane.on_close(move |_: &Pane, reason: &str| {
        // closing the pane ends the sign-in, unless it's an older pane a new open replaced
        let mine = lock(&xx.signins).get(&ix).and_then(|s| s.pane.as_ref().map(|p| p.id() == pid)).unwrap_or(false);
        if mine {
            signin_end(&xx, &ix, if reason.is_empty() { "closed" } else { reason }, false);
        }
    });
    let site = lock(&x.signins).get(&id).map(|s| s.site.clone()).unwrap_or_default();
    Ok(signin_opening(&site))
}

/// What the `signin` pane opens with: the page's size and rate, and `control`, so the apps offer Control on it.
fn signin_opening(site: &str) -> PaneOpenResponse {
    let video = Video {
        codec: video::Codec::CODEC_MJPEG.into(),
        width: signin::VIEWPORT.0 as i32,
        height: signin::VIEWPORT.1 as i32,
        fps: 10.0,
        ..Default::default()
    };
    let mut answer = PaneOpenResponse::video(video).titled(&format!("Sign in to {site}"));
    answer.control = Some(true);
    answer
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
    } else if std::env::var_os("POINTMAN_BROWSER_CLI").is_some()
        && !matches!(lock_try(&x.chrome_installing), Some(Some(_)))
    {
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
                None => {
                    let what = if is_nixos() && !x.setting_set("chrome") {
                        "building Chromium (nix, first call)"
                    } else {
                        "downloading Chrome"
                    };
                    part("chrome", "browser", "Chrome", State::Unspecified, vec![], what.into())
                }
                Some(Some(e)) => {
                    let hint = if is_nixos() { " It retries on the next call." } else { "" };
                    part("chrome", "browser", "Chrome", State::Failing, vec![Action::Update], format!("{e}.{hint}"))
                }
                Some(None) => part(
                    "chrome",
                    "browser",
                    "Chrome",
                    State::Unspecified,
                    vec![Action::Update],
                    if is_nixos() && !x.setting_set("chrome") {
                        "Chromium is built by nix at the first call".into()
                    } else {
                        "Chrome downloads at the first call".into()
                    },
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
        signins: Mutex::new(HashMap::new()),
        used: Mutex::new(HashMap::new()),
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
            signin_leftovers(&xs);
        });
        let xl = xs_sweep.clone();
        std::thread::spawn(move || loop {
            lane_sweep(&xl);
            signin_sweep(&xl);
            std::thread::sleep(Duration::from_secs(sweep_secs()));
        });
    });

    type Cmd = fn(&Ext, &Job) -> anyhow::Result<Value>;
    let commands: [(&str, Cmd); 6] = [
        ("browser.add", add),
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

    let xs = x.clone();
    ext.on(move |req: CredentialsSignInRequest| credentials_sign_in(&xs, &req));
    let xc = x.clone();
    ext.on(move |req: CredentialsCodeRequest| credentials_code(&xc, &req));
    let xp = x.clone();
    ext.pane("signin", move |pane: &Pane| signin_open(&xp, pane));

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
    fn the_sign_in_pane_is_one_people_may_control() {
        let a = signin_opening("stage.example.dev");
        assert_eq!(a.control, Some(true));
        assert_eq!(a.title.as_deref(), Some("Sign in to stage.example.dev"));
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
    fn status_says_chromium_is_not_built_yet_on_nixos() {
        assert_eq!(chrome_text(&json!({"kind": "not_built"})), "Chromium not built yet (builds on the first call)");
        assert_eq!(
            chrome_text(&json!({"kind": "system", "path": "/s/chromium/bin/chromium"})),
            "Chrome (system) /s/chromium/bin/chromium"
        );
        assert_eq!(chrome_text(&json!({"kind": "downloaded", "version": "157.0.8103.38"})), "Chrome 157.0.8103.38");
        assert_eq!(chrome_text(&json!({"kind": "none"})), "Chrome not installed yet");
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
