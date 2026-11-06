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
