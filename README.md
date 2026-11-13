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
| `browser.run` | `args`, `agent`, `doing?`, `timeout?`, `login?` | `agent-browser --session <agent> --json <args>`, headless; the JSON back with `ok`, `text` and `machine`. With `login`, the session is signed in first and `{{NAME}}` in `args` is the login's value |
| `browser.status` | | agent-browser's version, the Chrome it uses, and the open sessions |
| `browser.test_start` | `name`, `checklist`, `agent`, `login?` | A run from a markdown checklist (one `- ` line a step, an optional `expect:` under it); `{run}` back. Holds the machine's heavy-job lane, if the `lock` setting names a script |
| `browser.test_step` | `run`, `step`, `ok`, `note?`, `shot?`, `file?` | One step's result, with a still of the agent's browser session, or of `file` (a PNG or JPEG) |
| `browser.test_finish` | `run` | `report.html` (from `templates/report.html`, one still a step, failures first) as an output, with `{passed, failed, skipped, steps}`; lets go of the lane |

A run is kept in the extension's state folder (`runs/<id>/`) for a week. A run an agent leaves open is closed, and its
lane let go, when that agent starts another.

- **Errors are answers.** A missing element or failed click comes back as `ok: false` with `error.message`. The job
  only fails when agent-browser can't run, crashes, times out, or the call isn't allowed.
- **An allowlist, not a denylist:** only open, snapshot, click, dblclick, fill, type, press, keyboard, hover, focus,
  select, check, uncheck, scroll, scrollintoview, wait, get (not `cdp-url`), is, find, tab, back, forward, reload,
  screenshot, eval, close (not `--all`), console, errors, highlight and mouse, with the flags they take. Anything
  else, including what a later agent-browser adds, is refused. `file:`, `chrome:`, `javascript:` and `data:` URLs are
  refused, and a screenshot's path must be inside the job's own folder. `--headed` needs the "Visible window" setting.
- **Screenshots are posted.** A `screenshot` comes back as a PNG output, and the answer names the file.

## Test logins

`/browser add <label>` opens an open form (`open = true`, no fields of its own): the person names each value, shown or
hidden, and the apps seal them as one vault record, `browser.<label>`:

```json
{"kind": "env", "vars": {"URL": "https://stage.example.dev", "EMAIL": "bot@example.dev", "PASSWORD": "…"}, "plain": ["EMAIL", "URL"]}
```

`plain` lists the shown names; any other is hidden. A record from 0.1.6's form has no `plain`: its `URL`, `EMAIL`
and `ACCESS_CLIENT_ID` are shown, as they were then. Hidden values are at least 8 characters, so scrubbing one
never blanks short words in an answer.

- **Signing in** (`src/login.rs`), in this order:
  1. `STATE` (a saved session): written to a private file in the job's folder, `state load`, the file deleted, then
     `URL` opened.
  2. `URL` + `EMAIL` or `USERNAME` + `PASSWORD`: agent-browser's `auth save` with the password on stdin, `auth login`,
     then `auth delete`, whatever happened.
  3. `URL` alone: opened.

  `ACCESS_CLIENT_ID` + `ACCESS_CLIENT_SECRET` (both or neither) go as Cloudflare Access headers in a private config
  file, scoped by agent-browser to the URL's origin. The answer carries `signed_in` and `vars` (names, hidden marked).
- **`{{NAME}}`** (`src/vars.rs`): shown values go anywhere. A hidden value goes only in the text argument of `fill`,
  `type`, `keyboard type` or `keyboard inserttext`, only while the page's origin is the record's `URL` origin (checked
  with `get url` just before), and that command goes on agent-browser's stdin (`batch --json --omit-command`), never
  its argv. An unknown name, `{{NAME}}` without a login, and a value that would read as a flag are refused by name.
  Hidden values are scrubbed from every answer, error and overflow file.

## Sign in yourself

For sites a form can't sign in to (codes, SSO, emailed links): the form's Sign-in page row goes to
`credentials.sign_in` as `start` (https only).

1. **Start:** a session of the extension's own, `signin-<sign-in id>`, opens `start`. No agent's session can have
   that name (`browser.run` refuses it). It answers `{"pane": {"open": "signin", "target": {"session": "<id>"}},
   "prompt": "…"}`.
2. **The `signin` pane** (video, mjpeg): frames from agent-browser's stream (its WebSocket on 127.0.0.1), only while
   a viewer is attached. Only the person who started the sign-in can open it.
3. **Control:** while the node's grant holds, EXT-57's inputs go to the stream's input: `pointer` (0–1 of the
   picture) as mouse events, `scroll` as the wheel (`dy > 0` up), `key` (a macOS key code) as a key with its DOM code,
   and `text` as typed characters. On Linux, cmd is Ctrl, so copy, paste and select-all work. When control ends,
   anything still held down is let go.
4. **Save:** `credentials.code` with `"save"` runs `state save` and answers the record as the value:
   `{"kind":"env","vars":{"URL":"<origin at save>","STATE":"<state JSON>"},"plain":["URL"]}`. A page that isn't on
   an https site yet keeps the sign-in open.
5. **The vault's cap is 64 KB a value.** A bigger session keeps only the cookies that go to the URL's host and that
   host's own storage, and the answer's `note` says so. Too big even then is an error.
6. **It ends** with Save, the pane's close, or 15 minutes with no input: the session is closed and its folder
   (`signins/<session>` in the state folder) deleted. Sign-ins a stopped extension left are cleared when it starts.

## Development

`cargo test` runs against a fake agent-browser. `cargo test --test live -- --ignored --nocapture` runs the real
one by hand. A release is a tag that matches `extension.toml`'s version (`.github/workflows/release.yml`).
