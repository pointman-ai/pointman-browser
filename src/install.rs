//! agent-browser on a machine: one pinned release of github.com/vercel-labs/agent-browser for this platform (a
//! single executable, `agent-browser-<os>-<arch>`), checked against the sha256 this file carries, and put in
//! `<state>/agent-browser/<VERSION>/agent-browser`. No npm, no Homebrew, no sudo, nothing outside that folder.
//!
//! The hashes are the release's own asset digests (`gh release view v<VERSION> --json assets`). Linux builds are
//! the musl ones: static, so they start on NixOS too. A new version is a change to this file: VERSION and all four
//! hashes together.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use sha2::{Digest, Sha256};

pub const VERSION: &str = "0.41.2";

/// Each platform's asset and its sha256, from the v0.41.2 release.
const BUILDS: [(&str, &str); 4] = [
    ("darwin-arm64", "19680f06cf87d0095c318a2e80bea6b71523d95d3b651cd18d81885cd182477a"),
    ("darwin-x64", "fc55608c2222650e6e0d0154f9edebd203b63bf853b85d22240bfbddbb006ea1"),
    ("linux-musl-x64", "5d452d57772330fe4cef9b7aeb5e7137890dde20bbb65e82b990e0bc0b35d9cd"),
    ("linux-musl-arm64", "730a5168153e27ad1243ebe960157e69de0f24531fbbac9a040856a491bf491f"),
];

pub const RELEASES: &str = "https://github.com/vercel-labs/agent-browser/releases/download";

/// The executable is some 8 to 12 MB; anything much bigger isn't it.
const FILE_MAX: u64 = 128 * 1024 * 1024;

/// This platform's build, as agent-browser's releases name it.
pub fn target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") => Some("linux-musl-x64"),
        ("linux", "aarch64") => Some("linux-musl-arm64"),
        _ => None,
    }
}

/// The sha256 this extension trusts for `target`'s executable.
pub fn sha256_for(target: &str) -> Option<&'static str> {
    BUILDS.iter().find(|(t, _)| *t == target).map(|(_, sum)| *sum)
}

pub fn asset_name(target: &str) -> String {
    format!("agent-browser-{target}")
}

/// The pinned CLI, if it's installed: `<state>/agent-browser/<VERSION>/agent-browser`.
pub fn installed(state: &Path) -> Option<PathBuf> {
    let cli = state.join("agent-browser").join(VERSION).join("agent-browser");
    cli.is_file().then_some(cli)
}

fn download(url: &str, max: u64) -> anyhow::Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("pointman-browser/", env!("CARGO_PKG_VERSION")))
        .build();
    let r = agent.get(url).call().with_context(|| format!("downloading {url}"))?;
    let mut out = Vec::new();
    r.into_reader().take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        bail!("{url} is bigger than it should be");
    }
    Ok(out)
}

/// Download this platform's pinned executable from `releases` (agent-browser's, or a test's) and install it.
pub fn install(state: &Path, releases: &str) -> anyhow::Result<PathBuf> {
    let target = target().context("agent-browser has no build for this platform")?;
    let want = sha256_for(target).context("no pinned sha256 for this platform")?;
    let name = asset_name(target);
    let bytes = download(&format!("{}/v{VERSION}/{name}", releases.trim_end_matches('/')), FILE_MAX)?;
    install_bytes(state, &bytes, &name, want)
}

/// Check `bytes` against `want`, then put it at `<state>/agent-browser/<VERSION>/agent-browser`, all at once (a
/// half-written folder never has the final name), and remove other versions'.
pub fn install_bytes(state: &Path, bytes: &[u8], name: &str, want: &str) -> anyhow::Result<PathBuf> {
    let got = format!("{:x}", Sha256::digest(bytes));
    if !got.eq_ignore_ascii_case(want) {
        bail!("{name} doesn't match the sha256 this extension pins for agent-browser {VERSION}: not installed");
    }
    let root = state.join("agent-browser");
    fs::create_dir_all(&root)?;
    let part = root.join(format!(".{VERSION}.part"));
    let _ = fs::remove_dir_all(&part);
    fs::create_dir_all(&part)?;
    let path = part.join("agent-browser");
    fs::write(&path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    let to = root.join(VERSION);
    let _ = fs::remove_dir_all(&to);
    fs::rename(&part, &to)?;
    // older versions' folders: a command host still running from one keeps its open file
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_name() != VERSION && entry.path().is_dir() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(to.join("agent-browser"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn every_platform_has_a_pinned_build() {
        for (os, arch) in [("macos", "aarch64"), ("macos", "x86_64"), ("linux", "x86_64"), ("linux", "aarch64")] {
            let t = target_for(os, arch).unwrap();
            let s = sha256_for(t).unwrap();
            assert!(s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()), "{t}: {s}");
        }
        assert_eq!(target_for("windows", "x86_64"), None);
        assert!(target().is_some());
        assert_eq!(asset_name("linux-musl-x64"), "agent-browser-linux-musl-x64");
    }

    #[test]
    fn installs_the_executable() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("agent-browser/0.9.0")).unwrap();
        let a = b"#!/bin/sh\n";
        let cli = install_bytes(state.path(), a, "a", &sum(a)).unwrap();
        assert_eq!(cli, state.path().join("agent-browser/0.41.2/agent-browser"));
        assert_eq!(std::fs::read(&cli).unwrap(), a);
        assert!(!state.path().join("agent-browser/0.9.0").exists(), "an older version is removed");
        assert_eq!(installed(state.path()), Some(cli.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cli).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn a_wrong_hash_installs_nothing() {
        let state = tempfile::tempdir().unwrap();
        let e = install_bytes(state.path(), b"x", "a", &"0".repeat(64)).unwrap_err().to_string();
        assert!(e.contains("doesn't match the sha256"), "{e}");
        assert_eq!(installed(state.path()), None);
        assert!(!state.path().join("agent-browser").exists());
    }

    /// The real download from agent-browser's releases: `cargo test -- --ignored` (online).
    #[test]
    #[ignore]
    fn installs_the_pinned_release() {
        let state = tempfile::tempdir().unwrap();
        let cli = install(state.path(), RELEASES).unwrap();
        let out = std::process::Command::new(&cli).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains(VERSION), "{out:?}");
    }
}
