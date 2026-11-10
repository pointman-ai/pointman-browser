//! A test login the agent never sees: one vault item, `browser.login.<label>`, holding five `KEY=value` lines. The
//! extension reads it, sends the Cloudflare Access headers to that one origin, and signs in with the password on
//! agent-browser's stdin. No value goes into a job's params, an answer or a report.

use anyhow::{bail, Context};
use serde_json::Value;

pub const KEYS: [&str; 5] = ["URL", "ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET", "EMAIL", "PASSWORD"];

/// What a `[[credentials]]` form's value must match: each of the five keys on a line of its own with something after
/// it, and the URL on https. The extension parses strictly again at use, so this only catches a wrong paste early.
pub const FORM_PATTERN: &str = r"(?s)(?=.*(?:^|\n)\s*URL=https://\S)(?=.*(?:^|\n)\s*ACCESS_CLIENT_ID=\S)(?=.*(?:^|\n)\s*ACCESS_CLIENT_SECRET=\S)(?=.*(?:^|\n)\s*EMAIL=\S)(?=.*(?:^|\n)\s*PASSWORD=\S).+";

#[derive(Clone)]
pub struct Login {
    pub url: String,
    /// `https://host[:port]`: the one origin the Access headers go to.
    pub origin: String,
    pub client_id: String,
    pub client_secret: String,
    pub email: String,
    pub password: String,
}

/// Never prints a value.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Login {{ origin: {}, .. }}", self.origin)
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
    Ok(format!("browser.login.{label}"))
}

impl Login {
    /// Strict: the five keys, once each, no others. The text is the pasted lines, or a vault record
    /// `{"kind":"env","vars":{…}}` of the same five. Errors name keys, never values.
    pub fn parse(text: &str) -> anyhow::Result<Login> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let trimmed = text.trim();
        if trimmed.starts_with('{') {
            let v: Value =
                serde_json::from_str(trimmed).map_err(|_| anyhow::anyhow!("the login isn't KEY=value lines"))?;
            let vars = v["vars"].as_object().context("the login record has no vars")?;
            for (k, val) in vars {
                pairs.push((k.clone(), val.as_str().context("a login value isn't text")?.to_string()));
            }
        } else {
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let (k, v) = line.split_once('=').context("a login line isn't KEY=value")?;
                pairs.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let get = |key: &str| -> anyhow::Result<String> {
            let mut found = pairs.iter().filter(|(k, _)| k == key);
            let v = found.next().with_context(|| format!("the login has no {key}"))?.1.clone();
            if found.next().is_some() {
                bail!("the login has {key} twice");
            }
            if v.is_empty() {
                bail!("the login's {key} is empty");
            }
            Ok(v)
        };
        let login = Login {
            url: get("URL")?,
            origin: String::new(),
            client_id: get("ACCESS_CLIENT_ID")?,
            client_secret: get("ACCESS_CLIENT_SECRET")?,
            email: get("EMAIL")?,
            password: get("PASSWORD")?,
        };
        if let Some((k, _)) = pairs.iter().find(|(k, _)| !KEYS.contains(&k.as_str())) {
            bail!("the login has a key it doesn't use: {k}");
        }
        let origin = origin_of(&login.url)?;
        Ok(Login { origin, ..login })
    }

    /// The headers Cloudflare Access wants, as agent-browser's `--headers` JSON (scoped by it to the origin).
    pub fn access_headers(&self) -> String {
        serde_json::json!({"CF-Access-Client-Id": self.client_id, "CF-Access-Client-Secret": self.client_secret})
            .to_string()
    }

    fn secrets(&self) -> [&str; 4] {
        [&self.client_id, &self.client_secret, &self.email, &self.password]
    }

    /// `value` with every secret of this login replaced, in every string, however deep: an answer that echoes one
    /// (agent-browser echoes typed text) never carries it out.
    pub fn scrub(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                for secret in self.secrets() {
                    if s.contains(secret) {
                        *s = s.replace(secret, "***");
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|v| self.scrub(v)),
            Value::Object(o) => o.iter_mut().for_each(|(_, v)| self.scrub(v)),
            _ => {}
        }
    }

    /// The same for plain text (an error message).
    pub fn scrub_text(&self, text: &str) -> String {
        let mut v = Value::String(text.to_string());
        self.scrub(&mut v);
        v.as_str().unwrap_or_default().to_string()
    }
}

/// `https://host[:port]` of an https URL; anything else is refused.
pub fn origin_of(url: &str) -> anyhow::Result<String> {
    let rest = url.strip_prefix("https://").context("the login's URL must start with https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') || host.contains(char::is_whitespace) {
        bail!("the login's URL has no usable host");
    }
    Ok(format!("https://{host}"))
}

/// Whether `url` is on `origin` exactly: scheme, host and port.
pub fn same_origin(url: &str, origin: &str) -> bool {
    origin_of(url).is_ok_and(|o| o.eq_ignore_ascii_case(origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASTE: &str = "URL=https://stage.example.dev/sign-in\nACCESS_CLIENT_ID=abc.access\nACCESS_CLIENT_SECRET=s3cr3t-token\nEMAIL=bot@example.com\nPASSWORD=hunter2 pass\n";

    #[test]
    fn five_lines_parse_and_the_origin_comes_from_the_url() {
        let l = Login::parse(PASTE).unwrap();
        assert_eq!(l.origin, "https://stage.example.dev");
        assert_eq!(l.password, "hunter2 pass");
        assert!(!format!("{l:?}").contains("hunter2"));
    }

    #[test]
    fn an_env_record_of_the_same_five_parses() {
        let rec = serde_json::json!({"kind":"env","vars":{"URL":"https://a.test/x","ACCESS_CLIENT_ID":"i","ACCESS_CLIENT_SECRET":"s","EMAIL":"e@x.y","PASSWORD":"p"}});
        assert_eq!(Login::parse(&rec.to_string()).unwrap().origin, "https://a.test");
    }

    #[test]
    fn a_bad_paste_is_refused_without_its_values() {
        for (text, why) in [
            (PASTE.replace("EMAIL=bot@example.com\n", ""), "no EMAIL"),
            (format!("{PASTE}EXTRA=1\n"), "doesn't use: EXTRA"),
            (format!("{PASTE}EMAIL=again@x.y\n"), "EMAIL twice"),
            (PASTE.replace("https://", "http://"), "https://"),
            (PASTE.replace("PASSWORD=hunter2 pass", "PASSWORD="), "PASSWORD is empty"),
            ("just words".to_string(), "KEY=value"),
        ] {
            let e = format!("{:#}", Login::parse(&text).unwrap_err());
            assert!(e.contains(why), "{e} should say {why}");
            for secret in ["s3cr3t-token", "hunter2", "bot@example.com"] {
                assert!(!e.contains(secret), "{e} leaked {secret}");
            }
        }
    }

    #[test]
    fn the_form_pattern_wants_all_five_keys() {
        // the manifest carries this pattern (tests/manifest.rs); each key has a lookahead of its own
        for k in KEYS {
            assert!(FORM_PATTERN.contains(&format!("{k}=")), "{k}");
        }
    }

    #[test]
    fn scrub_removes_every_secret_at_any_depth() {
        let l = Login::parse(PASTE).unwrap();
        let mut v = serde_json::json!({"a": "typed hunter2 pass in", "b": [{"c": "id abc.access is not secret", "d": "bot@example.com"}], "e": 3});
        l.scrub(&mut v);
        let s = v.to_string();
        for secret in ["hunter2 pass", "bot@example.com", "s3cr3t-token"] {
            assert!(!s.contains(secret), "{s}");
        }
        assert!(s.contains("***"));
    }

    #[test]
    fn same_origin_checks_scheme_host_and_port() {
        assert!(same_origin("https://stage.example.dev/a", "https://stage.example.dev"));
        assert!(!same_origin("https://stage.example.dev:8443/a", "https://stage.example.dev"));
        assert!(!same_origin("http://stage.example.dev/a", "https://stage.example.dev"));
        assert!(!same_origin("https://evil.example/a", "https://stage.example.dev"));
    }

    #[test]
    fn labels_are_plain() {
        assert_eq!(item_name("stagesheep").unwrap(), "browser.login.stagesheep");
        for bad in ["", "../x", "a b", "-x", "x*"] {
            assert!(item_name(bad).is_err(), "{bad}");
        }
    }
}
