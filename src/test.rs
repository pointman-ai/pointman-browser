//! A UI test run: a markdown checklist, one step per `- ` line with an optional `expect:` line under it, each step
//! recorded pass or fail with a still, then one `report.html` made from `templates/report.html`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use base64::Engine;
use serde_json::{json, Value};

pub const TEMPLATE: &str = include_str!("../templates/report.html");
pub const MAX_STEPS: usize = 200;
/// A still attached with `file` is at most this big.
pub const STILL_MAX: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub text: String,
    pub expect: Option<String>,
}

/// The checklist's steps: each `- ` line (any indent), with the `expect: …` line under it.
pub fn parse_checklist(markdown: &str) -> anyhow::Result<Vec<Step>> {
    let mut steps: Vec<Step> = Vec::new();
    for line in markdown.lines() {
        let t = line.trim();
        if let Some(text) = t.strip_prefix("- ").map(str::trim).filter(|s| !s.is_empty()) {
            steps.push(Step { text: text.to_string(), expect: None });
        } else if let Some(e) = t.strip_prefix("expect:") {
            if let (Some(last), false) = (steps.last_mut(), e.trim().is_empty()) {
                last.expect = Some(e.trim().to_string());
            }
        }
    }
    if steps.is_empty() {
        bail!("the checklist has no steps: one `- ` line for each, with an optional indented `expect: …` under it");
    }
    if steps.len() > MAX_STEPS {
        bail!("the checklist has {} steps; at most {MAX_STEPS}", steps.len());
    }
    Ok(steps)
}

/// A run's id is made here, and read back from callers: only these characters, so it stays in the runs folder.
pub fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// JSON for a `<script type="application/json">`: no `<` survives, so no `</script>` or `<!--` can end or confuse it.
pub fn json_for_script(v: &Value) -> String {
    v.to_string().replace('<', "\\u003c")
}

/// "3 min 12 s", "45 s", "1 h 2 min".
pub fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min {} s", secs / 60, secs % 60),
        _ => format!("{} h {} min", secs / 3600, (secs % 3600) / 60),
    }
}

/// An epoch second as "2026-11-06 09:41 UTC".
pub fn utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let rem = epoch % 86_400;
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60)
}

/// The still at `path` as a `data:` URL, by what its first bytes say it is.
pub fn data_url(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mime = still_mime(&bytes).context("not a PNG or JPEG")?;
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

pub fn still_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else {
        None
    }
}

/// What a run keeps on disk: `<state>/runs/<id>/run.json`, its stills beside it.
pub fn run_dir(state: &Path, id: &str) -> PathBuf {
    state.join("runs").join(id)
}

#[derive(Debug, PartialEq)]
pub struct Counts {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub fn counts(steps: &[Value]) -> Counts {
    let n = |ok: Option<bool>| steps.iter().filter(|s| s["ok"].as_bool() == ok).count();
    Counts { passed: n(Some(true)), failed: n(Some(false)), skipped: n(None) }
}

/// The headline: "<name>: N of M steps failed", or "<name>: all M steps passed" (and the skipped ones said).
pub fn headline(name: &str, c: &Counts) -> String {
    let total = c.passed + c.failed + c.skipped;
    if c.failed > 0 {
        format!("{name}: {} of {total} steps failed", c.failed)
    } else if c.skipped > 0 {
        format!("{name}: {} of {total} steps passed, {} not run", c.passed, c.skipped)
    } else {
        format!("{name}: all {total} steps passed")
    }
}

/// Who ran it, where, against what, when, and the first failure in a sentence.
pub fn lead(run: &Value) -> String {
    let mut s = format!(
        "Run by @{} on {}{}, {}.",
        run["agent"].as_str().unwrap_or("an agent"),
        run["machine"].as_str().unwrap_or("a machine"),
        match run["url"].as_str().filter(|u| !u.is_empty()) {
            Some(u) => format!(" against {u}"),
            None => String::new(),
        },
        utc(run["started"].as_u64().unwrap_or(0)),
    );
    if let Some(f) = run["steps"].as_array().and_then(|a| a.iter().find(|s| s["ok"] == false)) {
        s.push_str(&format!(" First failure: step {}, {}", f["n"], f["text"].as_str().unwrap_or("")));
        match f["note"].as_str().filter(|n| !n.is_empty()) {
            Some(n) => s.push_str(&format!(": {n}")),
            None => {
                if let Some(e) = f["expect"].as_str() {
                    s.push_str(&format!(" (expected: {e})"));
                }
            }
        }
        s.push('.');
    }
    s
}

/// The `<li>` items: what the run did not check.
pub fn not_checked(run: &Value) -> String {
    let mut items: Vec<String> = run["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["ok"].is_null())
        .map(|s| format!("Step {} was not run: {}", s["n"], s["text"].as_str().unwrap_or("")))
        .collect();
    let no_still =
        run["steps"].as_array().map_or(0, |a| a.iter().filter(|s| !s["ok"].is_null() && s["shot"].is_null()).count());
    if no_still > 0 {
        items.push(format!("{no_still} recorded step{} have no still", if no_still == 1 { "" } else { "s" }));
    }
    items.push("Only the steps on the checklist: nothing else on the page or the site was looked at.".into());
    items.push("One browser: headless Chrome, at its default window size.".into());
    items.iter().map(|i| format!("<li>{}</li>", escape_html(i))).collect::<Vec<_>>().join("")
}

/// The report: the template, its leading how-to comment removed (it names every placeholder, and would carry the
/// stills twice), each placeholder filled.
pub fn render(run: &Value, took_secs: u64) -> String {
    let c = counts(run["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]));
    let name = run["name"].as_str().unwrap_or("UI test");
    let html = match (TEMPLATE.find("<!--"), TEMPLATE.find("-->")) {
        (Some(a), Some(b)) if a < b => format!("{}{}", &TEMPLATE[..a], TEMPLATE[b + 3..].trim_start_matches('\n')),
        _ => TEMPLATE.to_string(),
    };
    let fill = [
        ("{{TITLE}}", escape_html(&format!("{name}: UI test report"))),
        ("{{HEADLINE}}", escape_html(&headline(name, &c))),
        ("{{LEAD}}", escape_html(&lead(run))),
        ("{{PASSED}}", c.passed.to_string()),
        ("{{FAILED}}", c.failed.to_string()),
        ("{{SKIPPED}}", c.skipped.to_string()),
        ("{{TOOK}}", duration(took_secs)),
        ("{{NOT_CHECKED}}", not_checked(run)),
    ];
    let mut out = html;
    for (k, v) in fill {
        out = out.replace(k, &v);
    }
    // last, so nothing in the run's own words is read as a placeholder
    out.replace("{{RUN_JSON}}", &json_for_script(run))
}

pub fn new_run(id: &str, name: &str, agent: &str, machine: &str, started: u64, steps: &[Step]) -> Value {
    json!({
        "id": id, "name": name, "agent": agent, "machine": machine, "url": null, "started": started, "finished": null,
        "steps": steps.iter().enumerate().map(|(i, s)| json!({
            "n": i + 1, "text": s.text, "expect": s.expect, "ok": null, "note": null, "shot": null,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checklist_is_steps_with_expectations() {
        let md = "# Checkout\n\nSteps:\n- Open the home page\n  expect: the title says Spacesheep\n- Click Sign in\n- \n- Type the test email\n    expect:   a code field appears  \n";
        let steps = parse_checklist(md).unwrap();
        assert_eq!(
            steps,
            [
                Step { text: "Open the home page".into(), expect: Some("the title says Spacesheep".into()) },
                Step { text: "Click Sign in".into(), expect: None },
                Step { text: "Type the test email".into(), expect: Some("a code field appears".into()) },
            ]
        );
        assert!(parse_checklist("just prose\nexpect: x").is_err());
        assert!(parse_checklist(&"- s\n".repeat(201)).is_err());
    }

    #[test]
    fn run_ids_cannot_leave_the_runs_folder() {
        assert!(valid_run_id("run-1730882000-1"));
        for bad in ["", "../x", "a/b", "RUN", "a b", &"x".repeat(65)] {
            assert!(!valid_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn times_read_plainly() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(192), "3 min 12 s");
        assert_eq!(duration(3720), "1 h 2 min");
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_794_045_600), "2026-11-07 10:00 UTC");
        assert_eq!(utc(951_782_400 + 3600 * 12 + 60 * 5), "2000-02-29 12:05 UTC");
    }

    #[test]
    fn headline_and_lead_say_what_happened() {
        let mut run = new_run(
            "run-1",
            "Sign in",
            "patchbay",
            "jamess-macbook-air",
            1_794_045_600,
            &parse_checklist("- Open\n  expect: a form\n- Submit").unwrap(),
        );
        run["url"] = json!("https://staging.example/");
        run["steps"][0]["ok"] = json!(true);
        run["steps"][1]["ok"] = json!(false);
        run["steps"][1]["note"] = json!("the button was disabled");
        let c = counts(run["steps"].as_array().unwrap());
        assert_eq!(c, Counts { passed: 1, failed: 1, skipped: 0 });
        assert_eq!(headline("Sign in", &c), "Sign in: 1 of 2 steps failed");
        assert_eq!(
            lead(&run),
            "Run by @patchbay on jamess-macbook-air against https://staging.example/, 2026-11-07 10:00 UTC. First failure: step 2, Submit: the button was disabled."
        );
        assert_eq!(headline("X", &Counts { passed: 3, failed: 0, skipped: 0 }), "X: all 3 steps passed");
        assert_eq!(headline("X", &Counts { passed: 2, failed: 0, skipped: 1 }), "X: 2 of 3 steps passed, 1 not run");
    }

    #[test]
    fn the_report_is_filled_escaped_and_has_each_still_once() {
        let mut run = new_run(
            "run-1",
            "A <b>&\"name\"",
            "a",
            "m",
            0,
            &parse_checklist("- Say </script><script>alert(1)</script>\n- Two").unwrap(),
        );
        run["steps"][0]["ok"] = json!(true);
        run["steps"][0]["shot"] = json!("data:image/png;base64,AAAA");
        run["steps"][0]["note"] = json!("{{TITLE}} <!-- x -->");
        let html = render(&run, 192);
        let page = &html[..html.find("id=\"run\">").unwrap()];
        assert!(!page.contains("{{"), "no placeholder is left outside the run's own words");
        assert!(html.contains("A &lt;b&gt;&amp;&quot;name&quot;: UI test report"), "title escaped");
        assert!(html.contains("3 min 12 s"));
        assert_eq!(html.matches("data:image/png;base64,AAAA").count(), 1, "the how-to comment doesn't carry it twice");
        let start = html.find("id=\"run\">").unwrap() + 9;
        let json_text = &html[start..html[start..].find("</script>").unwrap() + start];
        let back: Value = serde_json::from_str(json_text).unwrap();
        assert_eq!(back["steps"][0]["text"], "Say </script><script>alert(1)</script>", "it reads back whole");
        assert!(!json_text.contains('<'));
        assert!(html.contains("Step 2 was not run: Two"));
        assert!(!html.contains("<!--\n  The report of one UI test run"), "the how-to comment is gone");
    }

    #[test]
    fn stills_are_png_or_jpeg() {
        assert_eq!(still_mime(b"\x89PNG\r\n\x1a\nxx"), Some("image/png"));
        assert_eq!(still_mime(b"\xff\xd8\xffxx"), Some("image/jpeg"));
        assert_eq!(still_mime(b"<html>"), None);
    }
}
