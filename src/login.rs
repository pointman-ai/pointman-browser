//! A test login the agent never sees: one vault item, `browser.<label>`, the record the apps seal from the form,
//! `{"kind":"env","vars":{NAME: value, …},"plain":["EMAIL","URL"]}`. The person names each value, shown (`plain`) or
//! hidden; a sign-in from the pane seals `{"vars":{"URL":…,"STATE":…},"plain":["URL"]}`. The extension reads it and
//! signs the agent's session in: from a saved STATE, else with URL + EMAIL (or USERNAME) + PASSWORD, else by opening
//! the URL. No hidden value goes into a job's params, an answer or a report.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context};
use serde_json::{json, Value};

/// The most values a record has, as the form allows.
pub const VARS_MAX: usize = 24;

/// The shortest hidden value: scrubbing a shorter one would blank those letters all over an answer.
pub const MIN_SECRET: usize = 8;

/// A value's name, as the form makes it: `^[A-Z][A-Z0-9_]{0,63}$`.
pub fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// How a session is signed in with a login, in MB-926's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// A saved session (STATE): agent-browser `state load`, then the URL.
    State,
    /// URL + EMAIL or USERNAME + PASSWORD: agent-browser's `auth` sign-in.
    Password,
    /// URL alone: opened.
    Open,
    /// None of them: the values are only for `{{NAME}}`.
    None,
}

impl How {
    pub fn word(self) -> &'static str {
        match self {
            How::State => "state",
            How::Password => "password",
            How::Open => "open",
            How::None => "none",
        }
    }
}

#[derive(Clone)]
pub struct Login {
    vars: BTreeMap<String, String>,
    plain: BTreeSet<String>,
    /// `https://host[:port]` of URL: the one origin the Access headers go to, and where hidden values may be typed.
    pub origin: Option<String>,
    /// The Access service token, both halves or neither: (client id, client secret).
    pub access: Option<(String, String)>,
}

/// Never prints a value.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Login {{ origin: {:?}, names: {:?}, .. }}", self.origin, self.vars.keys().collect::<Vec<_>>())
    }
}

/// A label is what an agent names: letters, digits, `.`, `_`, `-`.
pub fn item_name(label: &str) -> anyhow::Result<String> {
    let ok = !label.is_empty()
        && label.len() <= 32
        && label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !label.starts_with(['.', '-', '_']);
    if !ok {
        bail!("login is the label `/browser add` was given (letters, digits, . _ -), such as stagesheep");
    }
    Ok(format!("browser.{label}"))
}

impl Login {
    /// The vault record `{"kind":"env","vars":{…},"plain":[…]}`: names as the form makes them, text values, at most
    /// VARS_MAX. Errors name keys, never values.
    pub fn parse(text: &str) -> anyhow::Result<Login> {
        let v: Value =
            serde_json::from_str(text.trim()).map_err(|_| anyhow::anyhow!("the login isn't a vault record"))?;
        let given = v["vars"].as_object().context("the login record has no vars")?;
        if given.len() > VARS_MAX {
            bail!("the login has {} values: at most {VARS_MAX}", given.len());
        }
        for (k, val) in given {
            if !valid_name(k) {
                bail!("the login has a name that isn't A-Z, 0-9 and _ (starting with a letter): {}", clip_name(k));
            }
            if !val.is_string() {
                bail!("the login's {k} isn't text");
            }
        }
        let plain: BTreeSet<String> = match &v["plain"] {
            // no `plain`: every value is hidden (MB-926 §2)
            Value::Null => BTreeSet::new(),
            Value::Array(names) => names
                .iter()
                .map(|n| n.as_str().map(String::from).context("the login's plain is a list of names"))
                .collect::<anyhow::Result<_>>()?,
            _ => bail!("the login's plain is a list of names"),
        };
        // an empty row isn't stored, so an empty value is the same as none. Hidden values are used exactly as stored:
        // trimming only decides whether one is empty. Shown ones are trimmed.
        let mut vars = BTreeMap::new();
        for (k, val) in given {
            let raw = val.as_str().unwrap_or_default();
            if raw.trim().is_empty() {
                continue;
            }
            let shown = plain.contains(k);
            if !shown && raw.chars().count() < MIN_SECRET {
                bail!("the login's {k} is hidden and shorter than {MIN_SECRET} characters: make it Shown, or longer");
            }
            vars.insert(k.clone(), if shown { raw.trim().to_string() } else { raw.to_string() });
        }
        if vars.is_empty() {
            bail!("the login has no values");
        }
        let plain: BTreeSet<String> = plain.into_iter().filter(|n| vars.contains_key(n)).collect();
        let origin = vars.get("URL").map(|u| origin_of(u)).transpose()?;
        if let Some(state) = vars.get("STATE") {
            let ok = serde_json::from_str::<Value>(state).is_ok_and(|s| s["cookies"].is_array());
            if !ok {
                bail!("the login's STATE isn't a saved browser session");
            }
        }
        let access = match (vars.get("ACCESS_CLIENT_ID"), vars.get("ACCESS_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some((id.clone(), secret.clone())),
            (None, None) => None,
            (Some(_), None) => bail!("the login has ACCESS_CLIENT_ID but no ACCESS_CLIENT_SECRET: both or neither"),
            (None, Some(_)) => bail!("the login has ACCESS_CLIENT_SECRET but no ACCESS_CLIENT_ID: both or neither"),
        };
        if access.is_some() && origin.is_none() {
            bail!("the login has a Cloudflare Access token but no URL: the headers go only to the URL's site");
        }
        Ok(Login { vars, plain, origin, access })
    }

    /// A value by its name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Whether it has the value and it's hidden.
    pub fn hidden(&self, name: &str) -> bool {
        self.vars.contains_key(name) && !self.plain.contains(name)
    }

    /// Its names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.vars.keys().map(String::as_str).collect()
    }

    /// What an answer says of it: `[{name, hidden}]`, never a value.
    pub fn vars_answer(&self) -> Value {
        Value::Array(self.vars.keys().map(|n| json!({"name": n, "hidden": self.hidden(n)})).collect())
    }

    pub fn url(&self) -> Option<&str> {
        self.get("URL")
    }

    /// The account: EMAIL, else USERNAME.
    pub fn user(&self) -> Option<&str> {
        self.get("EMAIL").or_else(|| self.get("USERNAME"))
    }

    pub fn password(&self) -> Option<&str> {
        self.get("PASSWORD")
    }

    pub fn state(&self) -> Option<&str> {
        self.get("STATE")
    }

    /// STATE first, then URL + EMAIL/USERNAME + PASSWORD, then URL alone.
    pub fn how(&self) -> How {
        if self.state().is_some() {
            How::State
        } else if self.url().is_some() && self.user().is_some() && self.password().is_some() {
            How::Password
        } else if self.url().is_some() {
            How::Open
        } else {
            How::None
        }
    }

    /// The headers Cloudflare Access wants, as agent-browser's `--headers` JSON (scoped by it to the origin); none for a
    /// site without Access.
    pub fn access_headers(&self) -> Option<String> {
        let (id, secret) = self.access.as_ref()?;
        Some(json!({"CF-Access-Client-Id": id, "CF-Access-Client-Secret": secret}).to_string())
    }

    /// The hidden values, longest first, so one that holds another is replaced whole.
    fn secrets(&self) -> Vec<&str> {
        let mut s: Vec<&str> =
            self.vars.iter().filter(|(k, _)| !self.plain.contains(*k)).map(|(_, v)| v.as_str()).collect();
        s.sort_by_key(|v| std::cmp::Reverse(v.len()));
        s
    }

    /// `value` with every hidden value of this login replaced, in every string, however deep: an answer that echoes
    /// one (agent-browser echoes typed text) never carries it out.
    pub fn scrub(&self, value: &mut Value) {
        let secrets = self.secrets();
        scrub_with(&secrets, value);
    }

    /// The same for plain text (an error message).
    pub fn scrub_text(&self, text: &str) -> String {
        let mut v = Value::String(text.to_string());
        self.scrub(&mut v);
        v.as_str().unwrap_or_default().to_string()
    }
}

fn scrub_with(secrets: &[&str], value: &mut Value) {
    match value {
        Value::String(s) => {
            for secret in secrets {
                if s.contains(secret) {
                    *s = s.replace(secret, "***");
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| scrub_with(secrets, v)),
        Value::Object(o) => o.iter_mut().for_each(|(_, v)| scrub_with(secrets, v)),
        _ => {}
    }
}

/// A name from a record, shown in an error: only its first letters, and only the letters a name may have.
fn clip_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(24).collect();
    if n.is_empty() {
        "(no name)".into()
    } else {
        n
    }
}

/// `https://host[:port]` of an https URL; anything else is refused.
pub fn origin_of(url: &str) -> anyhow::Result<String> {
    let rest = url.strip_prefix("https://").context("the login's URL must start with https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') || host.contains(char::is_whitespace) {
        bail!("the login's URL has no usable host");
    }
    Ok(format!("https://{}", host.to_ascii_lowercase()))
}

/// Whether `url` is on `origin` exactly: scheme, host and port.
pub fn same_origin(url: &str, origin: &str) -> bool {
    origin_of(url).is_ok_and(|o| o.eq_ignore_ascii_case(origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(vars: Value, plain: &[&str]) -> String {
        json!({"kind": "env", "vars": vars, "plain": plain}).to_string()
    }

    fn full() -> Value {
        json!({"URL": "https://stage.example.dev/sign-in", "EMAIL": "bot@example.com", "PASSWORD": "hunter2 pass",
            "ACCESS_CLIENT_ID": "abc.access", "ACCESS_CLIENT_SECRET": "s3cr3t-token"})
    }

    const SHOWN: [&str; 3] = ["ACCESS_CLIENT_ID", "EMAIL", "URL"];

    fn without(keys: &[&str]) -> String {
        let mut v = full();
        for k in keys {
            v.as_object_mut().unwrap().remove(*k);
        }
        rec(v, &SHOWN)
    }

    fn state() -> String {
        json!({"cookies": [{"name": "sid", "value": "cookie-value-1", "domain": "stage.example.dev"}], "origins": []})
            .to_string()
    }

    #[test]
    fn a_record_with_access_parses_and_the_origin_comes_from_the_url() {
        let l = Login::parse(&rec(full(), &SHOWN)).unwrap();
        assert_eq!(l.origin.as_deref(), Some("https://stage.example.dev"));
        assert_eq!(l.password(), Some("hunter2 pass"));
        let h = l.access_headers().unwrap();
        assert!(h.contains("CF-Access-Client-Id") && h.contains("s3cr3t-token"));
        assert!(!format!("{l:?}").contains("hunter2"));
        assert_eq!(l.how(), How::Password);
    }

    #[test]
    fn the_sign_in_is_state_then_password_then_the_url_alone() {
        let mut v = full();
        v["STATE"] = state().into();
        assert_eq!(Login::parse(&rec(v, &SHOWN)).unwrap().how(), How::State);
        assert_eq!(Login::parse(&rec(full(), &SHOWN)).unwrap().how(), How::Password);
        let mut user = full();
        user.as_object_mut().unwrap().remove("EMAIL");
        user["USERNAME"] = "bot-user".into();
        let l = Login::parse(&rec(user, &["URL", "USERNAME", "ACCESS_CLIENT_ID"])).unwrap();
        assert_eq!((l.how(), l.user()), (How::Password, Some("bot-user")));
        assert_eq!(Login::parse(&without(&["PASSWORD"])).unwrap().how(), How::Open);
        assert_eq!(Login::parse(&without(&["EMAIL"])).unwrap().how(), How::Open);
        let only = rec(json!({"API_TOKEN": "tok-123456789"}), &[]);
        assert_eq!(Login::parse(&only).unwrap().how(), How::None);
    }

    #[test]
    fn names_are_the_persons_own_and_plain_says_which_are_shown() {
        let r =
            rec(json!({"URL": "https://x.example/", "TEAM": "blue", "OTP_SEED": "JBSWY3DPEHPK3PXP"}), &["TEAM", "URL"]);
        let l = Login::parse(&r).unwrap();
        assert_eq!(l.names(), ["OTP_SEED", "TEAM", "URL"]);
        assert!(l.hidden("OTP_SEED") && !l.hidden("TEAM") && !l.hidden("URL") && !l.hidden("NOPE"));
        assert_eq!(
            l.vars_answer(),
            json!([{"name": "OTP_SEED", "hidden": true}, {"name": "TEAM", "hidden": false}, {"name": "URL", "hidden": false}])
        );
        assert!(!l.vars_answer().to_string().contains("JBSWY3"), "names only");
    }

    #[test]
    fn a_name_not_in_plain_is_hidden_and_a_record_without_plain_is_all_hidden() {
        let r = json!({"kind": "env", "vars": full()}).to_string();
        let l = Login::parse(&r).unwrap();
        assert!(l.names().iter().all(|k| l.hidden(k)), "no plain: every value is hidden");
        let l = Login::parse(&rec(full(), &[])).unwrap();
        assert!(l.names().iter().all(|k| l.hidden(k)), "an empty plain: every value is hidden");
    }

    #[test]
    fn a_record_without_access_parses_and_sends_no_headers() {
        let l = Login::parse(&without(&["ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET"])).unwrap();
        assert!(l.access.is_none() && l.access_headers().is_none());
        // an optional row left empty isn't stored, and an empty one is the same as none
        let mut v = full();
        v["ACCESS_CLIENT_ID"] = "".into();
        v["ACCESS_CLIENT_SECRET"] = "".into();
        assert!(Login::parse(&rec(v, &SHOWN)).unwrap().access.is_none());
    }

    #[test]
    fn hidden_values_keep_their_spaces_and_shown_ones_are_trimmed() {
        let mut v = full();
        v["URL"] = " https://stage.example.dev/sign-in ".into();
        v["EMAIL"] = " bot@example.com ".into();
        v["PASSWORD"] = " hunter2 pass ".into();
        v["ACCESS_CLIENT_ID"] = " abc.access ".into();
        v["ACCESS_CLIENT_SECRET"] = " s3cr3t-token ".into();
        let l = Login::parse(&rec(v, &SHOWN)).unwrap();
        assert_eq!((l.url(), l.user()), (Some("https://stage.example.dev/sign-in"), Some("bot@example.com")));
        assert_eq!(l.password(), Some(" hunter2 pass "));
        assert_eq!(l.access, Some(("abc.access".to_string(), " s3cr3t-token ".to_string())));
        // a value of only spaces is still empty
        let mut v = full();
        v["PASSWORD"] = "        ".into();
        let l = Login::parse(&rec(v, &SHOWN)).unwrap();
        assert!(l.password().is_none() && !l.names().contains(&"PASSWORD"));
    }

    #[test]
    fn one_access_field_alone_is_refused() {
        for drop in ["ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET"] {
            let e = Login::parse(&without(&[drop])).unwrap_err().to_string();
            assert!(e.contains("both or neither") && e.contains(drop), "{e}");
        }
        let e = Login::parse(&without(&["URL"])).unwrap_err().to_string();
        assert!(e.contains("no URL"), "Access with nowhere to send it: {e}");
    }

    #[test]
    fn a_short_hidden_value_is_refused_but_a_short_shown_one_is_not() {
        for k in ["PASSWORD", "ACCESS_CLIENT_SECRET"] {
            let mut v = full();
            v[k] = "abc".into();
            let e = Login::parse(&rec(v, &SHOWN)).unwrap_err().to_string();
            assert!(e.contains(k) && e.contains("shorter") && !e.contains("abc"), "{e}");
        }
        let mut v = full();
        v["EMAIL"] = "a@b.c".into();
        v["ACCESS_CLIENT_ID"] = "id".into();
        Login::parse(&rec(v, &SHOWN)).unwrap();
    }

    #[test]
    fn only_the_vault_record_is_read() {
        let lines = "URL=https://stage.example.dev/sign-in\nEMAIL=bot@example.com\nPASSWORD=hunter2 pass\n";
        assert!(Login::parse(lines).is_err(), "KEY=value lines are not read");
    }

    #[test]
    fn a_bad_record_is_refused_without_its_values() {
        let mut http = full();
        http["URL"] = "http://stage.example.dev/".into();
        let mut lower = full();
        lower["lower"] = "1".into();
        let mut num = full();
        num["EMAIL"] = 3.into();
        let mut bad_state = full();
        bad_state["STATE"] = "not a state s3cr3t-token".into();
        let many: serde_json::Map<String, Value> = (0..25).map(|i| (format!("V{i}"), json!("value-long-1"))).collect();
        for (text, why) in [
            (rec(http, &SHOWN), "https://"),
            (rec(lower, &SHOWN), "A-Z"),
            (rec(num, &SHOWN), "EMAIL isn't text"),
            (rec(bad_state, &SHOWN), "STATE isn't a saved browser session"),
            (rec(Value::Object(many), &[]), "at most 24"),
            (json!({"kind": "env", "vars": full(), "plain": "URL"}).to_string(), "plain is a list"),
            (rec(json!({}), &[]), "no values"),
            ("{\"kind\":\"env\"}".to_string(), "no vars"),
            ("just words".to_string(), "vault record"),
        ] {
            let e = format!("{:#}", Login::parse(&text).unwrap_err());
            assert!(e.contains(why), "{e} should say {why}");
            for secret in ["s3cr3t-token", "hunter2", "bot@example.com"] {
                assert!(!e.contains(secret), "{e} leaked {secret}");
            }
        }
    }

    #[test]
    fn scrub_removes_the_hidden_values_at_any_depth_and_leaves_the_shown_ones() {
        let mut v = full();
        v["STATE"] = state().into();
        let l = Login::parse(&rec(v, &SHOWN)).unwrap();
        let mut v = json!({"a": "typed hunter2 pass in", "b": [{"c": "id abc.access is plain", "d": "bot@example.com"}],
                           "e": 3, "f": "s3cr3t-token", "g": state()});
        l.scrub(&mut v);
        let s = v.to_string();
        for secret in ["hunter2 pass", "s3cr3t-token", "cookie-value-1"] {
            assert!(!s.contains(secret), "{s}");
        }
        assert!(s.contains("abc.access") && s.contains("bot@example.com"), "shown values are not scrubbed: {s}");
        assert!(s.contains("***"));
    }

    #[test]
    fn same_origin_checks_scheme_host_and_port() {
        assert!(same_origin("https://stage.example.dev/a", "https://stage.example.dev"));
        assert!(same_origin("https://STAGE.example.dev/a", "https://stage.example.dev"));
        assert!(!same_origin("https://stage.example.dev:8443/a", "https://stage.example.dev"));
        assert!(!same_origin("http://stage.example.dev/a", "https://stage.example.dev"));
        assert!(!same_origin("https://evil.example/a", "https://stage.example.dev"));
        assert!(!same_origin("https://stage.example.dev.evil.example/", "https://stage.example.dev"));
    }

    #[test]
    fn labels_are_plain() {
        assert_eq!(item_name("stagesheep").unwrap(), "browser.stagesheep");
        for bad in ["", "../x", "a b", "-x", "x*"] {
            assert!(item_name(bad).is_err(), "{bad}");
        }
    }
}
