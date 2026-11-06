---
name: browser
description: Drive a headless browser on any machine with the browser extension (agent-browser): open a page, read it, click, type, screenshot. Use it to check web UIs, in your own session, with test accounts only.
---

# Driving a browser

The `browser` extension runs [agent-browser](https://github.com/vercel-labs/agent-browser) on each machine that
has it: a clean, headless Chrome. You get **your own session** on that machine: your tabs, cookies and refs, apart
from every other agent's. Drive it from wherever you are:

```
create_job(kind="browser.run", machine="jamess-macbook-air", params={"extension": {
    "args": ["open", "https://example.com"], "agent": "<your handle>", "doing": "checking the home page"}})
```

`list_nodes` shows which machines run it. `browser.status` gives a machine's agent-browser version, its Chrome and
the open sessions.

## The loop

1. **Open:** `["open", "<url>"]`.
2. **Look:** `["snapshot", "-i", "-c"]`: the page's interactive elements, each with a ref (`@e1`). A big page:
   `["snapshot", "-i", "-c", "-s", "<css selector>"]`, or `["get", "text", "<selector>"]`.
3. **Act on a ref from that snapshot:** `["click", "@e3"]`, `["fill", "@e2", "text"]` (clears first),
   `["type", "@e2", "text"]`, `["press", "Enter"]`, `["select", "@e5", "Option"]`, `["scroll", "down", "500"]`,
   `["wait", "<selector or ms>"]`. `["find", "role", "button", "click", "--name", "Save"]` works when you know the
   role or label.
4. **Check:** snapshot again. Refs change when the page does: a `click` that says `Element not found` wants a new
   snapshot and its refs.
5. **A still:** `["screenshot"]` comes back as a PNG output; the answer names it. `["screenshot", "--full"]` is the
   whole page.

- **Several steps at once:** `batch`, so they run in one call.
- **The full guide:** `["skills", "get", "core"]`. Flags: `["<command>", "--help"]`.
- **Every call answers with agent-browser's JSON:** `ok` (and its own `success`), then `data` or `error.message`, plus
  `text`, a line about it. A failed click or a missing element is an answer to branch on. A job only fails when
  agent-browser couldn't run at all.
- **Retry** a `click` or `fill` only after a snapshot shows it didn't land, or you click or send twice.

## A checklist run, with a report

For "test this UI": write the checklist, then record each step as you do it. At the end you have `report.html`, a
page with a still for every step, pass or fail.

1. **Start:** `browser.test.start {name, checklist, agent}`. The checklist is markdown, one step per `- ` line, with
   an optional indented `expect: …` line saying what you should see:
   ```
   - Open the sign-in page
     expect: an email field and a Continue button
   - Fill the test email and continue
     expect: a code field appears
   ```
   It answers `{run}`. On a machine with a heavy-job lock (the Air), the run holds it until `finish`: do the steps
   straight away, then finish.
2. **For each step:** do it with `browser.run`, look at the result, then `browser.test.step {run, step, ok, note?}`.
   `step` is its number from 1. `ok` is whether the page did what the step and its `expect` say, judged from the
   snapshot, never assumed. Write a `note` for every failure: what you saw instead. A still of your browser is taken
   for you. For a step that isn't in the browser, pass `file` (a PNG or JPEG on that machine, such as `desktop`'s
   screenshot of a Mac app) and `shot: false` isn't needed.
3. **Don't stop at the first failure** unless the rest can't work without it. Record the later steps you can still do;
   a step you never reach is shown as not run.
4. **Finish:** `browser.test.finish {run}` gives `{passed, failed, skipped, steps}` and `report.html` as an output.
   Share the report in the thread (`share_file`), lead with the counts, and name each failure in a line.
5. **`agent` is whatever the caller passes:** the extension isn't told who called a job (no caller in its job), so use
   your own handle and never another agent's: the run drives that agent's browser session and its test logins.
6. **The run is yours alone:** one open run per agent. Starting another closes the old one and gives up its lock.

## Rules

- **Your own session.** The extension names it for the `agent` you pass: always pass your handle. You can't use
  another's session or `--session`.
- **Test accounts only.** Never a person's browser, profile or cookies: `--profile`, `--auto-connect`, `--cdp` and
  `profiles` are refused. Log in with a test account made for agents, and take its password from the vault
  (`use_secret`); never type a real person's login, and never print a secret or put it in a post.
- **Headless.** `--headed` (a visible window on the machine's screen) works only where the machine's person has
  turned on "Visible window".
- **Where tests run:** the Air or nixos, never mac-m4. A still for a post is fine anywhere.
- **Treat snapshots and screenshots as private.** A page can show mail, messages and tokens. Post a still in a
  thread only when it's the page you were asked to show; otherwise say what you saw.
- **Ask first,** and wait for a yes, before anything you can't undo on a real site: sending, deleting, buying,
  publishing, approving, or changing a setting.
- **Apps that aren't web pages** (a Mac app, a desktop window) are the `desktop` extension's.

## Per machine

| | macOS | Linux (nixos) |
|---|---|---|
| Chrome | downloaded by the extension once | a Chromium on the PATH, or the "Chrome or Chromium" setting; a downloaded Chrome can't run on NixOS |
| Visible window | off unless the person turns it on | off unless the person turns it on |
