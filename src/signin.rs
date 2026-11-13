//! Sign in yourself (EXT-156): `/browser add` starts a sign-in session of the extension's own, never an agent's, shown in
//! the `signin` video pane with Control. The person signs in (codes, SSO and emailed links all work), taps Save, and
//! the session's cookies and storage (agent-browser `state save`) are sealed as the login, with the URL's origin.
//!
//! This file is what can be decided without a browser: the session's name, what each of EXT-57's inputs is as
//! agent-browser's stream input, a frame's size, and the record that goes in the vault.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Every sign-in session's name starts with this; `browser.run` refuses an agent whose session would.
pub const PREFIX: &str = "signin-";
/// A sign-in nobody touches for this long ends, and its files go.
pub const IDLE_SECS: u64 = 15 * 60;
/// The most sign-ins open at once on a machine.
pub const OPEN_MAX: usize = 3;
/// The vault's cap on a value (pointman-vault's MAX_BYTES).
pub const VALUE_MAX: usize = 64 * 1024;
/// The longest paste at once.
pub const TEXT_MAX: usize = 4000;
/// The most a note on a saved sign-in may be (CredentialsCodeResponse.note).
pub const NOTE_MAX: usize = 300;
/// The page's size before the first frame says otherwise: agent-browser's default viewport.
pub const VIEWPORT: (f64, f64) = (1280.0, 720.0);

/// The sign-in's session name from core's sign-in id: `signin-<id>`, in what agent-browser takes.
pub fn session_name(id: &str) -> Result<String> {
    let clean: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(48).collect();
    if clean.is_empty() {
        bail!("the sign-in has no id");
    }
    Ok(format!("{PREFIX}{clean}"))
}

/// Whether a session name is a sign-in's: none of an agent's may be.
pub fn is_signin_session(name: &str) -> bool {
    name.len() >= PREFIX.len() && name[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

/// What the form shows beside the pane.
pub fn prompt(start: &str) -> String {
    let host = start.trim_start_matches("https://").split(['/', '?', '#']).next().unwrap_or_default();
    let p = format!("Sign in to {host} in the browser pane, then tap Save.");
    p.chars().take(200).collect()
}

// ------------------------------------------------------------------ the record

/// The record Save seals: `{"kind":"env","vars":{"URL":<origin>,"STATE":<state>},"plain":["URL"]}`, at most the vault's
/// cap. A bigger state keeps only what goes to the origin's host (its cookies, and its own storage), and the note says
/// so. Too big even then is an error, with the sizes (never a value).
pub fn record(origin: &str, state: &str) -> Result<(String, Option<String>)> {
    let state: Value = serde_json::from_str(state).map_err(|_| anyhow!("agent-browser's saved state isn't JSON"))?;
    let make = |state: &Value| {
        json!({"kind": "env", "vars": {"URL": origin, "STATE": state.to_string()}, "plain": ["URL"]}).to_string()
    };
    let whole = make(&state);
    if whole.len() <= VALUE_MAX {
        return Ok((whole, None));
    }
    let host = origin.trim_start_matches("https://").split(':').next().unwrap_or_default().to_ascii_lowercase();
    let mine = only_site(&state, &host);
    let kept = make(&mine);
    if kept.len() > VALUE_MAX {
        bail!(
            "the signed-in session is {} KB even with only {host}'s cookies and storage: the vault keeps at most {} KB",
            kept.len() / 1024,
            VALUE_MAX / 1024
        );
    }
    let note = format!(
        "The whole session was {} KB, over the vault's {} KB: only {host}'s cookies and storage were kept.",
        whole.len() / 1024,
        VALUE_MAX / 1024
    );
    // the protocol's note is at most 300 characters
    Ok((kept, Some(note.chars().take(NOTE_MAX).collect())))
}

/// A state with only the cookies that go to `host` (its own, and its parent domains') and `host`'s own storage.
pub fn only_site(state: &Value, host: &str) -> Value {
    let goes_to = |domain: &str| {
        let d = domain.trim_start_matches('.').to_ascii_lowercase();
        !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
    };
    let cookies: Vec<Value> = state["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["domain"].as_str().is_some_and(goes_to))
        .cloned()
        .collect();
    let origins: Vec<Value> = state["origins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| {
            o["origin"].as_str().is_some_and(|u| {
                let h = u.split("://").nth(1).unwrap_or_default().split(['/', ':']).next().unwrap_or_default();
                h.eq_ignore_ascii_case(host)
            })
        })
        .cloned()
        .collect();
    json!({"cookies": cookies, "origins": origins})
}

// ------------------------------------------------------------------ frames

/// A JPEG's width and height, from its start-of-frame marker.
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u16, u16)> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if matches!(marker, 0xD8 | 0x01 | 0xD0..=0xD7) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        // SOF0..SOF15, but not DHT (C4), JPG (C8) or DAC (CC)
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let s = jpeg.get(i + 5..i + 9)?;
            let (h, w) = (u16::from_be_bytes([s[0], s[1]]), u16::from_be_bytes([s[2], s[3]]));
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

// ------------------------------------------------------------------ control

/// What the pane's person controls: until when (unix seconds), what's held down, and where the pointer is.
#[derive(Debug, Default)]
pub struct Control {
    until: Option<u64>,
    held: Option<&'static str>,
    keys: BTreeSet<u16>,
    at: (f64, f64),
}

impl Control {
    /// A grant until a time, or None: control ended, and what's held is let go (its messages).
    pub fn set(&mut self, until: Option<u64>) -> Vec<Value> {
        let done = if until.is_none() { self.release() } else { Vec::new() };
        self.until = until;
        done
    }

    pub fn granted(&self, now: u64) -> bool {
        self.until.is_some_and(|u| now < u)
    }

    /// Everything still held down, let go.
    pub fn release(&mut self) -> Vec<Value> {
        let (x, y) = self.at;
        let mut out: Vec<Value> =
            self.held.take().map(|button| mouse("mouseReleased", x, y, Some(button), 1, 0)).into_iter().collect();
        for code in std::mem::take(&mut self.keys) {
            if let Ok(up) = key_message(code, None, 0, false) {
                out.push(up);
            }
        }
        out
    }

    /// One of EXT-57's inputs as agent-browser's stream messages, on a page `size` CSS pixels big, at `now` (unix
    /// seconds). `mac`: the browser runs on macOS, so cmd is Meta; elsewhere cmd is Ctrl, which is what copy, paste
    /// and select-all are there. Refused without a grant, after it, or when it isn't one.
    pub fn messages(&mut self, event: &Value, size: (f64, f64), now: u64, mac: bool) -> Result<Vec<Value>> {
        match self.until {
            None => bail!("control is off"),
            Some(u) if now >= u => bail!("control ran out: turn it on again"),
            Some(_) => {}
        }
        let place = |e: &Value| -> Result<(f64, f64)> {
            let f = |k: &str| e[k].as_f64().filter(|n| n.is_finite()).map(|n| n.clamp(0.0, 1.0));
            let (Some(x), Some(y)) = (f("x"), f("y")) else { bail!("x and y say where, each 0-1 of the picture") };
            Ok(((x * size.0 * 100.0).round() / 100.0, (y * size.1 * 100.0).round() / 100.0))
        };
        let mods = modifiers(&event["mods"], mac)?;
        Ok(match event["kind"].as_str() {
            Some("pointer") => {
                let (x, y) = place(event)?;
                self.at = (x, y);
                let button = match event["button"].as_str() {
                    None | Some("left") => "left",
                    Some("right") => "right",
                    Some(b) => bail!("button is left or right, not {b}"),
                };
                let clicks = event["clicks"].as_i64().unwrap_or(1).clamp(1, 3);
                match event["phase"].as_str() {
                    None | Some("move") => vec![mouse("mouseMoved", x, y, self.held, 0, mods)],
                    Some("down") => {
                        let mut out: Vec<Value> =
                            self.held.take().map(|b| mouse("mouseReleased", x, y, Some(b), 1, 0)).into_iter().collect();
                        self.held = Some(button);
                        out.push(mouse("mousePressed", x, y, Some(button), clicks, mods));
                        out
                    }
                    Some("up") => {
                        self.held = None;
                        vec![mouse("mouseReleased", x, y, Some(button), clicks, mods)]
                    }
                    Some(p) => bail!("phase is move, down or up, not {p}"),
                }
            }
            Some("scroll") => {
                let (x, y) = if event["x"].is_null() && event["y"].is_null() { self.at } else { place(event)? };
                self.at = (x, y);
                let px =
                    |k: &str| event[k].as_f64().filter(|n| n.is_finite()).unwrap_or(0.0).clamp(-10_000.0, 10_000.0);
                // EXT-57's dy above 0 scrolls up; Chrome's wheel deltaY above 0 scrolls down
                vec![json!({"type": "input_mouse", "eventType": "mouseWheel", "x": x, "y": y,
                            "deltaX": -px("dx"), "deltaY": -px("dy"), "modifiers": mods})]
            }
            Some("key") => {
                let code = event["code"]
                    .as_u64()
                    .filter(|c| *c < 128)
                    .ok_or_else(|| anyhow!("code is a macOS key code, 0-127"))? as u16;
                let chars = event["chars"].as_str().filter(|c| !c.is_empty());
                match event["phase"].as_str() {
                    Some("down") => {
                        self.keys.insert(code);
                        vec![key_message(code, chars, mods, true)?]
                    }
                    Some("up") => {
                        self.keys.remove(&code);
                        vec![key_message(code, chars, mods, false)?]
                    }
                    None => vec![key_message(code, chars, mods, true)?, key_message(code, chars, mods, false)?],
                    Some(p) => bail!("phase is down or up, not {p}"),
                }
            }
            Some("text") => typed(event["text"].as_str().ok_or_else(|| anyhow!("text is the text to type"))?)?,
            Some(k) => bail!("{k} isn't an input the sign-in pane takes"),
            None => bail!("an input without a kind"),
        })
    }
}

fn mouse(event: &str, x: f64, y: f64, button: Option<&str>, clicks: i64, mods: i64) -> Value {
    json!({"type": "input_mouse", "eventType": event, "x": x, "y": y, "button": button.unwrap_or("none"),
           "clickCount": clicks, "modifiers": mods})
}

/// Chrome's modifier bits: Alt 1, Ctrl 2, Meta 4, Shift 8.
const ALT: i64 = 1;
const CTRL: i64 = 2;
const META: i64 = 4;
const SHIFT: i64 = 8;

/// `mods` (cmd, opt or alt, ctrl, shift) as Chrome's bits.
fn modifiers(mods: &Value, mac: bool) -> Result<i64> {
    let mut bits = 0;
    for m in mods.as_array().map(Vec::as_slice).unwrap_or_default() {
        bits |= match m.as_str().unwrap_or_default() {
            "shift" => SHIFT,
            "ctrl" | "control" => CTRL,
            "opt" | "alt" | "option" => ALT,
            "cmd" | "command" if mac => META,
            "cmd" | "command" => CTRL,
            other => bail!("mods are cmd, opt, ctrl and shift, not {other}"),
        };
    }
    Ok(bits)
}

/// A key by its macOS virtual key code: the DOM code, the key's name (None for one that types its character), the
/// character it types unshifted, and Windows's key code, which Chrome wants for editing keys.
type Key = (u16, &'static str, Option<&'static str>, &'static str, i64);

const KEYS: &[Key] = &[
    (0, "KeyA", None, "a", 65),
    (1, "KeyS", None, "s", 83),
    (2, "KeyD", None, "d", 68),
    (3, "KeyF", None, "f", 70),
    (4, "KeyH", None, "h", 72),
    (5, "KeyG", None, "g", 71),
    (6, "KeyZ", None, "z", 90),
    (7, "KeyX", None, "x", 88),
    (8, "KeyC", None, "c", 67),
    (9, "KeyV", None, "v", 86),
    (11, "KeyB", None, "b", 66),
    (12, "KeyQ", None, "q", 81),
    (13, "KeyW", None, "w", 87),
    (14, "KeyE", None, "e", 69),
    (15, "KeyR", None, "r", 82),
    (16, "KeyY", None, "y", 89),
    (17, "KeyT", None, "t", 84),
    (18, "Digit1", None, "1", 49),
    (19, "Digit2", None, "2", 50),
    (20, "Digit3", None, "3", 51),
    (21, "Digit4", None, "4", 52),
    (22, "Digit6", None, "6", 54),
    (23, "Digit5", None, "5", 53),
    (24, "Equal", None, "=", 187),
    (25, "Digit9", None, "9", 57),
    (26, "Digit7", None, "7", 55),
    (27, "Minus", None, "-", 189),
    (28, "Digit8", None, "8", 56),
    (29, "Digit0", None, "0", 48),
    (30, "BracketRight", None, "]", 221),
    (31, "KeyO", None, "o", 79),
    (32, "KeyU", None, "u", 85),
    (33, "BracketLeft", None, "[", 219),
    (34, "KeyI", None, "i", 73),
    (35, "KeyP", None, "p", 80),
    (36, "Enter", Some("Enter"), "\r", 13),
    (37, "KeyL", None, "l", 76),
    (38, "KeyJ", None, "j", 74),
    (39, "Quote", None, "'", 222),
    (40, "KeyK", None, "k", 75),
    (41, "Semicolon", None, ";", 186),
    (42, "Backslash", None, "\\", 220),
    (43, "Comma", None, ",", 188),
    (44, "Slash", None, "/", 191),
    (45, "KeyN", None, "n", 78),
    (46, "KeyM", None, "m", 77),
    (47, "Period", None, ".", 190),
    (48, "Tab", Some("Tab"), "", 9),
    (49, "Space", None, " ", 32),
    (50, "Backquote", None, "`", 192),
    (51, "Backspace", Some("Backspace"), "", 8),
    (53, "Escape", Some("Escape"), "", 27),
    (54, "MetaRight", Some("Meta"), "", 93),
    (55, "MetaLeft", Some("Meta"), "", 91),
    (56, "ShiftLeft", Some("Shift"), "", 16),
    (57, "CapsLock", Some("CapsLock"), "", 20),
    (58, "AltLeft", Some("Alt"), "", 18),
    (59, "ControlLeft", Some("Control"), "", 17),
    (60, "ShiftRight", Some("Shift"), "", 16),
    (61, "AltRight", Some("Alt"), "", 18),
    (62, "ControlRight", Some("Control"), "", 17),
    (65, "NumpadDecimal", None, ".", 110),
    (67, "NumpadMultiply", None, "*", 106),
    (69, "NumpadAdd", None, "+", 107),
    (75, "NumpadDivide", None, "/", 111),
    (76, "NumpadEnter", Some("Enter"), "\r", 13),
    (78, "NumpadSubtract", None, "-", 109),
    (81, "NumpadEqual", None, "=", 187),
    (82, "Numpad0", None, "0", 96),
    (83, "Numpad1", None, "1", 97),
    (84, "Numpad2", None, "2", 98),
    (85, "Numpad3", None, "3", 99),
    (86, "Numpad4", None, "4", 100),
    (87, "Numpad5", None, "5", 101),
    (88, "Numpad6", None, "6", 102),
    (89, "Numpad7", None, "7", 103),
    (91, "Numpad8", None, "8", 104),
    (92, "Numpad9", None, "9", 105),
    (96, "F5", Some("F5"), "", 116),
    (97, "F6", Some("F6"), "", 117),
    (98, "F7", Some("F7"), "", 118),
    (99, "F3", Some("F3"), "", 114),
    (100, "F8", Some("F8"), "", 119),
    (101, "F9", Some("F9"), "", 120),
    (109, "F10", Some("F10"), "", 121),
    (111, "F12", Some("F12"), "", 123),
    (115, "Home", Some("Home"), "", 36),
    (116, "PageUp", Some("PageUp"), "", 33),
    (117, "Delete", Some("Delete"), "", 46),
    (119, "End", Some("End"), "", 35),
    (121, "PageDown", Some("PageDown"), "", 34),
    (122, "F1", Some("F1"), "", 112),
    (103, "F11", Some("F11"), "", 122),
    (118, "F4", Some("F4"), "", 115),
    (120, "F2", Some("F2"), "", 113),
    (123, "ArrowLeft", Some("ArrowLeft"), "", 37),
    (124, "ArrowRight", Some("ArrowRight"), "", 39),
    (125, "ArrowDown", Some("ArrowDown"), "", 40),
    (126, "ArrowUp", Some("ArrowUp"), "", 38),
];

fn key_of(code: u16) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.0 == code)
}

/// A key going down or up as agent-browser's `input_keyboard`: a key that types (with no Ctrl or Meta held) carries
/// its text, as the app gave it in `chars` (shifted, or an Option character) or else unshifted; one that doesn't is a
/// raw key down, as Chrome's own keyboard sends it.
pub fn key_message(code: u16, chars: Option<&str>, mods: i64, down: bool) -> Result<Value> {
    let &(_, dom, name, base, vk) = key_of(code).ok_or_else(|| anyhow!("key code {code} isn't one the pane knows"))?;
    let shortcut = mods & (CTRL | META) != 0;
    let (key, text): (String, Option<String>) = match name {
        // Enter types a return, unless it's a shortcut
        Some(n) => (n.to_string(), (!base.is_empty() && !shortcut).then(|| base.to_string())),
        None => {
            let typed = chars.filter(|c| c.chars().count() == 1 && !c.chars().any(char::is_control));
            let key = typed.unwrap_or(base).to_string();
            (key.clone(), (!shortcut).then_some(key))
        }
    };
    let event = match (down, &text) {
        (false, _) => "keyUp",
        (true, Some(_)) => "keyDown",
        (true, None) => "rawKeyDown",
    };
    let mut m = json!({"type": "input_keyboard", "eventType": event, "key": key, "code": dom,
                       "windowsVirtualKeyCode": vk, "modifiers": mods});
    if let (true, Some(t)) = (down, text) {
        m["text"] = json!(t);
    }
    Ok(m)
}

/// A paste as typed characters, with each line break as Return.
fn typed(text: &str) -> Result<Vec<Value>> {
    if text.chars().count() > TEXT_MAX {
        bail!("text is at most {TEXT_MAX} characters at once");
    }
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push(key_message(36, None, 0, true)?);
            out.push(key_message(36, None, 0, false)?);
        }
        for c in line.chars().filter(|c| *c != '\r' && !c.is_control()) {
            out.push(json!({"type": "input_keyboard", "eventType": "char", "text": c.to_string()}));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000;
    const SIZE: (f64, f64) = (1280.0, 720.0);

    fn granted() -> Control {
        let mut c = Control::default();
        c.set(Some(NOW + 60));
        c
    }

    #[test]
    fn a_sign_ins_session_is_never_an_agents() {
        assert_eq!(session_name("sin_42").unwrap(), "signin-sin_42");
        assert_eq!(session_name("a/b c").unwrap(), "signin-abc");
        assert!(session_name("//").is_err());
        assert!(is_signin_session("signin-x") && is_signin_session("SignIn-x"));
        assert!(!is_signin_session("patchbay") && !is_signin_session("signi"));
    }

    #[test]
    fn the_prompt_names_the_site() {
        assert_eq!(
            prompt("https://stage.example.dev/login?x=1"),
            "Sign in to stage.example.dev in the browser pane, then tap Save."
        );
    }

    #[test]
    fn the_record_is_the_url_and_the_state_with_url_shown() {
        let state = json!({"cookies": [{"name": "sid", "value": "v", "domain": "stage.example.dev"}], "origins": []});
        let (value, note) = record("https://stage.example.dev", &state.to_string()).unwrap();
        let v: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(v["kind"], "env");
        assert_eq!(v["plain"], json!(["URL"]));
        assert_eq!(v["vars"]["URL"], "https://stage.example.dev");
        assert_eq!(serde_json::from_str::<Value>(v["vars"]["STATE"].as_str().unwrap()).unwrap(), state);
        assert!(note.is_none());
        // and it's a login the extension reads, signing in from the state
        let l = crate::login::Login::parse(&value).unwrap();
        assert_eq!(l.how(), crate::login::How::State);
        assert!(l.hidden("STATE") && !l.hidden("URL"));
    }

    #[test]
    fn a_state_over_the_vaults_cap_keeps_only_the_sites_cookies_and_storage() {
        let big = "x".repeat(40_000);
        let state = json!({
            "cookies": [
                {"name": "sid", "value": "mine", "domain": "app.example.dev"},
                {"name": "parent", "value": "mine-too", "domain": ".example.dev"},
                {"name": "ads", "value": big, "domain": ".tracker.example"},
                {"name": "sso", "value": big, "domain": "sso.example.dev"},
            ],
            "origins": [
                {"origin": "https://app.example.dev", "localStorage": [{"name": "t", "value": "keep"}]},
                {"origin": "https://other.example", "localStorage": [{"name": "t", "value": big}]},
            ]
        });
        let (value, note) = record("https://app.example.dev", &state.to_string()).unwrap();
        assert!(value.len() <= VALUE_MAX, "{}", value.len());
        let note = note.unwrap();
        assert!(note.contains("app.example.dev") && note.contains("64 KB"), "{note}");
        let v: Value = serde_json::from_str(&value).unwrap();
        let kept: Value = serde_json::from_str(v["vars"]["STATE"].as_str().unwrap()).unwrap();
        let names: Vec<&str> =
            kept["cookies"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["sid", "parent"]);
        assert_eq!(kept["origins"].as_array().unwrap().len(), 1);
        // still too big: refused with sizes, never a value
        let huge = json!({"cookies": [{"name": "sid", "value": "y".repeat(80_000), "domain": "app.example.dev"}], "origins": []});
        let e = record("https://app.example.dev", &huge.to_string()).unwrap_err().to_string();
        assert!(e.contains("at most 64 KB") && !e.contains("yyyy"), "{e}");
    }

    #[test]
    fn a_jpegs_size_comes_from_its_frame_header() {
        // SOI, an APP0 segment, then SOF0 with height 2 and width 3
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x03, 0x01,
            0x01, 0x11, 0x00, 0xFF, 0xD9,
        ];
        assert_eq!(jpeg_size(&jpeg), Some((3, 2)));
        assert_eq!(jpeg_size(b"\x89PNG"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn input_needs_a_grant_that_hasnt_run_out() {
        let mut c = Control::default();
        let click = json!({"kind": "pointer", "x": 0.5, "y": 0.5, "phase": "down"});
        assert!(c.messages(&click, SIZE, NOW, false).unwrap_err().to_string().contains("control is off"));
        c.set(Some(NOW));
        assert!(c.messages(&click, SIZE, NOW, false).unwrap_err().to_string().contains("ran out"));
        assert!(granted().granted(NOW) && !Control::default().granted(NOW));
    }

    #[test]
    fn a_click_lands_where_it_points_on_the_page() {
        let mut c = granted();
        let down = c
            .messages(&json!({"kind": "pointer", "x": 0.25, "y": 0.5, "phase": "down", "clicks": 2}), SIZE, NOW, false)
            .unwrap();
        assert_eq!(
            down,
            [
                json!({"type": "input_mouse", "eventType": "mousePressed", "x": 320.0, "y": 360.0, "button": "left", "clickCount": 2, "modifiers": 0})
            ]
        );
        let drag =
            c.messages(&json!({"kind": "pointer", "x": 0.5, "y": 0.5, "phase": "move"}), SIZE, NOW, false).unwrap();
        assert_eq!(drag[0]["button"], "left", "a move while held drags");
        let up = c
            .messages(&json!({"kind": "pointer", "x": 0.5, "y": 0.5, "phase": "up", "clicks": 2}), SIZE, NOW, false)
            .unwrap();
        assert_eq!((up[0]["eventType"].as_str(), up[0]["clickCount"].as_i64()), (Some("mouseReleased"), Some(2)));
        let right = c
            .messages(
                &json!({"kind": "pointer", "x": 2, "y": -1, "phase": "down", "button": "right"}),
                SIZE,
                NOW,
                false,
            )
            .unwrap();
        assert_eq!(
            (right[0]["x"].as_f64(), right[0]["y"].as_f64(), right[0]["button"].as_str()),
            (Some(1280.0), Some(0.0), Some("right"))
        );
        assert!(c
            .messages(&json!({"kind": "pointer", "x": 0.1, "y": 0.1, "button": "middle"}), SIZE, NOW, false)
            .is_err());
    }

    #[test]
    fn scrolling_up_is_a_negative_wheel() {
        let mut c = granted();
        let m =
            c.messages(&json!({"kind": "scroll", "dx": 3, "dy": 40, "x": 0.5, "y": 0.5}), SIZE, NOW, false).unwrap();
        assert_eq!(
            (m[0]["eventType"].as_str(), m[0]["deltaY"].as_f64(), m[0]["deltaX"].as_f64()),
            (Some("mouseWheel"), Some(-40.0), Some(-3.0))
        );
        let at = c.messages(&json!({"kind": "scroll", "dy": -10}), SIZE, NOW, false).unwrap();
        assert_eq!(
            (at[0]["x"].as_f64(), at[0]["deltaY"].as_f64()),
            (Some(640.0), Some(10.0)),
            "no place: where the pointer was"
        );
    }

    #[test]
    fn keys_carry_their_text_and_shortcuts_dont() {
        let mut c = granted();
        let a = c
            .messages(
                &json!({"kind": "key", "code": 0, "chars": "A", "mods": ["shift"], "phase": "down"}),
                SIZE,
                NOW,
                false,
            )
            .unwrap();
        assert_eq!(
            a[0],
            json!({"type": "input_keyboard", "eventType": "keyDown", "key": "A", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": 8, "text": "A"})
        );
        let enter = key_message(36, None, 0, true).unwrap();
        assert_eq!(
            (enter["key"].as_str(), enter["text"].as_str(), enter["windowsVirtualKeyCode"].as_i64()),
            (Some("Enter"), Some("\r"), Some(13))
        );
        let back = key_message(51, None, 0, true).unwrap();
        assert_eq!(
            (back["eventType"].as_str(), back.get("text")),
            (Some("rawKeyDown"), None),
            "editing keys don't type"
        );
        // cmd+a: Ctrl on Linux, where select-all is Ctrl+A; Meta on a Mac
        let linux =
            c.messages(&json!({"kind": "key", "code": 0, "chars": "a", "mods": ["cmd"]}), SIZE, NOW, false).unwrap();
        assert_eq!((linux.len(), linux[0]["modifiers"].as_i64(), linux[0].get("text")), (2, Some(2), None));
        assert_eq!(linux[1]["eventType"], "keyUp");
        let mac =
            c.messages(&json!({"kind": "key", "code": 0, "mods": ["cmd"], "phase": "down"}), SIZE, NOW, true).unwrap();
        assert_eq!(mac[0]["modifiers"], 4);
        let arrow = key_message(126, None, 0, true).unwrap();
        assert_eq!((arrow["code"].as_str(), arrow["windowsVirtualKeyCode"].as_i64()), (Some("ArrowUp"), Some(38)));
        assert!(c.messages(&json!({"kind": "key", "code": 10}), SIZE, NOW, false).is_err(), "an unknown code");
        assert!(c.messages(&json!({"kind": "key", "code": 0, "mods": ["hyper"]}), SIZE, NOW, false).is_err());
    }

    #[test]
    fn a_paste_is_typed_with_returns_between_lines() {
        let mut c = granted();
        let m = c.messages(&json!({"kind": "text", "text": "ab\ncé"}), SIZE, NOW, false).unwrap();
        let kinds: Vec<&str> = m.iter().map(|e| e["eventType"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["char", "char", "keyDown", "keyUp", "char", "char"]);
        assert_eq!(m[5]["text"], "é");
        assert!(c.messages(&json!({"kind": "text", "text": "x".repeat(TEXT_MAX + 1)}), SIZE, NOW, false).is_err());
    }

    #[test]
    fn nothing_is_left_held_down_when_control_ends() {
        let mut c = granted();
        c.messages(
            &json!({"kind": "pointer", "x": 0.0, "y": 0.0, "phase": "down", "button": "right"}),
            SIZE,
            NOW,
            false,
        )
        .unwrap();
        c.messages(&json!({"kind": "key", "code": 56, "phase": "down"}), SIZE, NOW, false).unwrap();
        let let_go = c.set(None);
        let kinds: Vec<&str> = let_go.iter().map(|e| e["eventType"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["mouseReleased", "keyUp"]);
        assert_eq!(let_go[0]["button"], "right");
        assert!(c.set(None).is_empty());
    }

    #[test]
    fn every_key_code_is_known_once() {
        let mut seen = BTreeSet::new();
        for k in KEYS.iter() {
            assert!(seen.insert(k.0), "code {} twice", k.0);
        }
    }
}
