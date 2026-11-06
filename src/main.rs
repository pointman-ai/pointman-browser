//! The extension's process: the node starts it (`bin/browser`, from the extension's folder) and talks to it over
//! stdin and stdout (the SDK handles that). Log with `h.log(…)`: stdout is the node's.
//!
//! `browser.run` runs one agent-browser command on this machine for an agent anywhere (create_job with `machine`),
//! in a session named for that agent: its own tabs, cookies and refs, in a clean headless Chrome that's never a
//! person's browser. Each call runs on a thread of its own and can be cancelled: the CLI is killed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{bail, Context};
use browser::cli::{self, Call};
use browser::install;
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
        which(&SYSTEM_CHROME)
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
    if let Some(why) = cli::refused(&args) {
        bail!(why);
    }
    if args.iter().any(|a| a == "--headed") && !x.setting("headed", false) {
        bail!(
            "A visible browser window is off on {} (the Browser extension's \"Visible window\" setting): browse \
             headless, or ask this machine's person to turn it on.",
            x.h.machine()
        );
    }
    if job.params["agent"].as_str().map(str::trim).unwrap_or_default().is_empty() {
        bail!("agent is your handle: each agent has its own browser session, named for it");
    }
    let agent = cli::session_name(job.params["agent"].as_str());
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
    let x = Arc::new(Ext { h: ext.handle(), installing: Mutex::new(None), chrome_installing: Mutex::new(None) });

    // the pinned CLI and its Chrome are fetched as soon as the extension starts, so the first call doesn't wait
    let xs = x.clone();
    ext.on_start(move |_: &Handle| {
        let xs = xs.clone();
        std::thread::spawn(move || {
            if let Err(e) = xs.cli().and_then(|_| xs.ensure_chrome()) {
                xs.h.log(format!("{e:#}"));
            }
        });
    });

    type Cmd = fn(&Ext, &Job) -> anyhow::Result<Value>;
    let commands: [(&str, Cmd); 2] = [("browser.run", run), ("browser.status", status)];
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
    fn a_downloaded_chrome_is_found_by_its_folder() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(downloaded_chrome(home.path()), None);
        std::fs::create_dir_all(home.path().join(".agent-browser/browsers/chrome-157.0.8103.38")).unwrap();
        assert_eq!(downloaded_chrome(home.path()).as_deref(), Some("157.0.8103.38"));
    }
}
