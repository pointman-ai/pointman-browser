//! A test login the agent never sees: one vault item, `browser.<label>`, the record the apps seal from the form's
//! fields, `{"kind":"env","vars":{…}}`. The extension reads it, sends the Cloudflare Access headers (when the site has
//! them) to that one origin, and signs in with the password on agent-browser's stdin. No value goes into a job's
//! params, an answer or a report.

use anyhow::{bail, Context};
use serde_json::Value;

pub const KEYS: [&str; 5] = ["URL", "EMAIL", "PASSWORD", "ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET"];

/// The shortest PASSWORD or ACCESS_CLIENT_SECRET: scrubbing a shorter one would blank those letters all over an answer.
pub const MIN_SECRET: usize = 8;

#[derive(Clone)]
pub struct Login {
    pub url: String,
    /// `https://host[:port]`: the one origin the Access headers go to.
    pub origin: String,
    /// Plain: shown as typed in the form, so never scrubbed.
    pub email: String,
    pub password: String,
    /// The Access service token, both halves or neither: (client id, client secret).
    pub access: Option<(String, String)>,
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
    Ok(format!("browser.{label}"))
}

impl Login {
    /// Strict: the vault record `{"kind":"env","vars":{…}}` with URL, EMAIL and PASSWORD, and ACCESS_CLIENT_ID with
    /// ACCESS_CLIENT_SECRET or neither; no other keys. Errors name keys, never values.
    pub fn parse(text: &str) -> anyhow::Result<Login> {
        let v: Value =
            serde_json::from_str(text.trim()).map_err(|_| anyhow::anyhow!("the login isn't a vault record"))?;
        let vars = v["vars"].as_object().context("the login record has no vars")?;
        for (k, val) in vars {
            if !KEYS.contains(&k.as_str()) {
                bail!("the login has a key it doesn't use: {k}");
            }
            if !val.is_string() {
                bail!("the login's {k} isn't text");
            }
        }
        // an empty optional field isn't stored, so an empty value is the same as none
        // the hidden values (PASSWORD, ACCESS_CLIENT_SECRET) are used exactly as stored: trimming only decides whether
        // one is empty. The plain ones (URL, EMAIL, ACCESS_CLIENT_ID) are trimmed.
        let get = |key: &str| {
            let raw = vars.get(key).and_then(Value::as_str)?;
            if raw.trim().is_empty() {
                None
            } else if matches!(key, "PASSWORD" | "ACCESS_CLIENT_SECRET") {
                Some(raw)
            } else {
                Some(raw.trim())
            }
        };
        let need = |key: &str| get(key).map(String::from).with_context(|| format!("the login has no {key}"));
        let short = |key: &str, v: &str| {
            if v.chars().count() < MIN_SECRET {
                bail!("the login's {key} is shorter than {MIN_SECRET} characters");
            }
            Ok(())
        };
        let url = need("URL")?;
        let email = need("EMAIL")?;
        let password = need("PASSWORD")?;
        short("PASSWORD", &password)?;
        let access = match (get("ACCESS_CLIENT_ID"), get("ACCESS_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => {
                short("ACCESS_CLIENT_SECRET", secret)?;
                Some((id.to_string(), secret.to_string()))
            }
            (None, None) => None,
            (Some(_), None) => bail!("the login has ACCESS_CLIENT_ID but no ACCESS_CLIENT_SECRET: both or neither"),
            (None, Some(_)) => bail!("the login has ACCESS_CLIENT_SECRET but no ACCESS_CLIENT_ID: both or neither"),
        };
        let origin = origin_of(&url)?;
        Ok(Login { url, origin, email, password, access })
    }

    /// The headers Cloudflare Access wants, as agent-browser's `--headers` JSON (scoped by it to the origin); none for a
    /// site without Access.
    pub fn access_headers(&self) -> Option<String> {
        let (id, secret) = self.access.as_ref()?;
        Some(serde_json::json!({"CF-Access-Client-Id": id, "CF-Access-Client-Secret": secret}).to_string())
    }

    /// The hidden values: the password and the Access secret. The URL, email and client id are plain, shown as typed.
    fn secrets(&self) -> Vec<&str> {
        let mut s = vec![self.password.as_str()];
        s.extend(self.access.iter().map(|(_, secret)| secret.as_str()));
        s
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

    fn rec(vars: Value) -> String {
        serde_json::json!({"kind": "env", "vars": vars}).to_string()
    }

    fn full() -> Value {
        serde_json::json!({"URL": "https://stage.example.dev/sign-in", "EMAIL": "bot@example.com", "PASSWORD": "hunter2 pass",
            "ACCESS_CLIENT_ID": "abc.access", "ACCESS_CLIENT_SECRET": "s3cr3t-token"})
    }

    fn without(keys: &[&str]) -> String {
        let mut v = full();
        for k in keys {
            v.as_object_mut().unwrap().remove(*k);
        }
        rec(v)
    }

    #[test]
    fn a_record_with_access_parses_and_the_origin_comes_from_the_url() {
        let l = Login::parse(&rec(full())).unwrap();
        assert_eq!(l.origin, "https://stage.example.dev");
        assert_eq!(l.password, "hunter2 pass");
        let h = l.access_headers().unwrap();
        assert!(h.contains("CF-Access-Client-Id") && h.contains("s3cr3t-token"));
        assert!(!format!("{l:?}").contains("hunter2"));
    }

    #[test]
    fn a_record_without_access_parses_and_sends_no_headers() {
        let l = Login::parse(&without(&["ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET"])).unwrap();
        assert!(l.access.is_none() && l.access_headers().is_none());
        // an optional field left empty isn't stored, and an empty one is the same as none
        let mut v = full();
        v["ACCESS_CLIENT_ID"] = "".into();
        v["ACCESS_CLIENT_SECRET"] = "".into();
        assert!(Login::parse(&rec(v)).unwrap().access.is_none());
    }

    #[test]
    fn hidden_values_keep_their_spaces_and_plain_ones_are_trimmed() {
        let mut v = full();
        v["URL"] = " https://stage.example.dev/sign-in ".into();
        v["EMAIL"] = " bot@example.com ".into();
        v["PASSWORD"] = " hunter2 pass ".into();
        v["ACCESS_CLIENT_ID"] = " abc.access ".into();
        v["ACCESS_CLIENT_SECRET"] = " s3cr3t-token ".into();
        let l = Login::parse(&rec(v)).unwrap();
        assert_eq!((l.url.as_str(), l.email.as_str()), ("https://stage.example.dev/sign-in", "bot@example.com"));
        assert_eq!(l.password, " hunter2 pass ");
        assert_eq!(l.access, Some(("abc.access".to_string(), " s3cr3t-token ".to_string())));
        // a value of only spaces is still empty
        let mut v = full();
        v["PASSWORD"] = "        ".into();
        assert!(Login::parse(&rec(v)).unwrap_err().to_string().contains("no PASSWORD"));
    }

    #[test]
    fn one_access_field_alone_is_refused() {
        for drop in ["ACCESS_CLIENT_ID", "ACCESS_CLIENT_SECRET"] {
            let e = Login::parse(&without(&[drop])).unwrap_err().to_string();
            assert!(e.contains("both or neither") && e.contains(drop), "{e}");
        }
    }

    #[test]
    fn a_missing_url_email_or_password_is_refused_by_key() {
        for k in ["URL", "EMAIL", "PASSWORD"] {
            let e = Login::parse(&without(&[k])).unwrap_err().to_string();
            assert!(e.contains(&format!("no {k}")), "{e}");
        }
    }

    #[test]
    fn a_short_password_or_access_secret_is_refused_but_a_short_email_or_id_is_not() {
        for k in ["PASSWORD", "ACCESS_CLIENT_SECRET"] {
            let mut v = full();
            v[k] = "abc".into();
            let e = Login::parse(&rec(v)).unwrap_err().to_string();
            assert!(e.contains(k) && e.contains("shorter") && !e.contains("abc"), "{e}");
        }
        let mut v = full();
        v["EMAIL"] = "a@b.c".into();
        v["ACCESS_CLIENT_ID"] = "id".into();
        Login::parse(&rec(v)).unwrap();
    }

    #[test]
    fn only_the_vault_record_is_read() {
        let lines = "URL=https://stage.example.dev/sign-in\nEMAIL=bot@example.com\nPASSWORD=hunter2 pass\n";
        assert!(Login::parse(lines).is_err(), "KEY=value lines are no longer read");
    }

    #[test]
    fn a_bad_record_is_refused_without_its_values() {
        let mut http = full();
        http["URL"] = "http://stage.example.dev/".into();
        let mut extra = full();
        extra["EXTRA"] = "1".into();
        let mut num = full();
        num["EMAIL"] = 3.into();
        for (text, why) in [
            (rec(http), "https://"),
            (rec(extra), "doesn't use: EXTRA"),
            (rec(num), "EMAIL isn't text"),
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
    fn scrub_removes_the_hidden_values_at_any_depth_and_leaves_the_plain_ones() {
        let l = Login::parse(&rec(full())).unwrap();
        let mut v = serde_json::json!({"a": "typed hunter2 pass in", "b": [{"c": "id abc.access is plain", "d": "bot@example.com"}], "e": 3, "f": "s3cr3t-token"});
        l.scrub(&mut v);
        let s = v.to_string();
        for secret in ["hunter2 pass", "s3cr3t-token"] {
            assert!(!s.contains(secret), "{s}");
        }
        assert!(s.contains("abc.access") && s.contains("bot@example.com"), "plain values are not scrubbed: {s}");
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
        assert_eq!(item_name("stagesheep").unwrap(), "browser.stagesheep");
        for bad in ["", "../x", "a b", "-x", "x*"] {
            assert!(item_name(bad).is_err(), "{bad}");
        }
    }
}
