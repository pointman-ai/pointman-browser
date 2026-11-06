# browser

A Pointman extension: [agent-browser](https://github.com/vercel-labs/agent-browser) on each machine, so agents can
drive a headless Chrome there: open pages, read them as a tree of refs, click, type, take screenshots. An agent
anywhere sends a command to any machine with `create_job(kind="browser.run", machine=…)`. Each agent gets its own
session (`--session <agent>`): its own tabs, cookies and refs.

It pairs with [desktop](https://github.com/pointman-ai/pointman-desktop), which does the same for a Mac or Linux
app's controls.

One Rust binary, `bin/browser`, built for each platform with the
[pointman-extension](https://github.com/pointman-ai/pointman-extension) crate.

## agent-browser itself

The extension doesn't use npm or Homebrew. It installs one pinned agent-browser release into its state folder
(`agent-browser/<version>/agent-browser`): the single executable `agent-browser-<os>-<arch>` from agent-browser's
GitHub releases (Linux: the static musl builds). Before it is used, it's checked against a sha256 that
`src/install.rs` carries. Then `agent-browser install` downloads Chrome into the extension's own home folder.

- **Where the hashes come from:** the release's asset digests (`gh release view v<version> --json assets`).
- **A new version** is one change to `src/install.rs`: `VERSION` and all four hashes.
- **Its home is the extension's.** Every call runs with `HOME` set to `<state>/home`, so the machine's person's
  `~/.agent-browser`, Chrome profile and cookies are never read.
- **On Linux,** a downloaded Chrome doesn't run on NixOS. The extension uses a `chromium` on the PATH, or the path in
  its "Chrome or Chromium" setting.

## Commands

| Kind | Params | What |
|---|---|---|
| `browser.run` | `args`, `agent`, `doing?`, `timeout?` | `agent-browser --session <agent> --json <args>`, headless; the JSON back with `ok`, `text` and `machine` |
| `browser.status` | | agent-browser's version, the Chrome it uses, and the open sessions |
| `browser.test.start` | `name`, `checklist`, `agent` | A run from a markdown checklist (one `- ` line a step, an optional `expect:` under it); `{run}` back. Holds the machine's heavy-job lane, if the `lock` setting names a script |
| `browser.test.step` | `run`, `step`, `ok`, `note?`, `shot?`, `file?` | One step's result, with a still of the agent's browser session, or of `file` (a PNG or JPEG) |
| `browser.test.finish` | `run` | `report.html` (from `templates/report.html`, one still a step, failures first) as an output, with `{passed, failed, skipped, steps}`; lets go of the lane |

A run is kept in the extension's state folder (`runs/<id>/`) for a week. A run an agent leaves open is closed, and its
lane let go, when that agent starts another.

- **Errors are answers.** A missing element or failed click comes back as `ok: false` with `error.message`. The job
  only fails when agent-browser can't run, crashes, times out, or the call isn't allowed.
- **Refused:** `--profile`, `--auto-connect`, `--cdp`, `--session`, `--state`, `profiles`, `connect` and others
  that reach a person's browser or another agent's session. `--headed` needs the "Visible window" setting.
- **Screenshots are posted.** A `screenshot` comes back as a PNG output, and the answer names the file.

## Development

`cargo test` runs against a fake agent-browser. `cargo test --test live -- --ignored --nocapture` runs the real
one by hand. A release is a tag that matches `extension.toml`'s version (`.github/workflows/release.yml`).
