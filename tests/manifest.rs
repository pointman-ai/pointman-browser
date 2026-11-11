//! extension.toml within what core's manifest check takes (Pointman's src/messageboard/extensions/
//! manifest.py): core refuses a release whose manifest breaks one.

#[test]
fn the_manifest_keeps_to_cores_limits() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/extension.toml")).unwrap();
    let m: toml::Table = toml::from_str(&text).unwrap();
    let chars = |v: &toml::Value| v.as_str().unwrap().chars().count();
    let ext = &m["extension"];
    assert_eq!(ext["version"].as_str(), Some(env!("CARGO_PKG_VERSION")), "extension.toml's version is Cargo's");
    assert!((1..=60).contains(&chars(&ext["name"])) && (1..=200).contains(&chars(&ext["summary"])));
    for c in m["commands"].as_array().unwrap() {
        let kind = c["kind"].as_str().unwrap();
        // core's rule: the extension's id, one dot, then [a-z][a-z0-9_]* (manifest.py)
        let verb = kind.strip_prefix("browser.").unwrap_or_else(|| panic!("{kind} must start with browser."));
        assert!(
            verb.starts_with(|ch: char| ch.is_ascii_lowercase())
                && verb.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
            "command '{kind}' must be browser.<verb>, with one dot and the verb in [a-z][a-z0-9_]*"
        );
        assert!((1..=60).contains(&chars(&c["title"])), "{kind}'s title is {} characters", chars(&c["title"]));
        assert!(chars(&c["summary"]) <= 120, "{kind}'s summary is {} characters", chars(&c["summary"]));
        let slash = c["slash"].as_str().unwrap();
        let ok = slash.len() <= 24
            && slash.starts_with(|ch: char| ch.is_ascii_lowercase())
            && slash.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        assert!(ok, "{kind}'s slash {slash}");
        assert_eq!(c["params"]["type"].as_str(), Some("object"), "{kind}");
    }
    // the vault: only the test logins, and the form is core's one value with an item named for the extension (manifest.py)
    let secrets: Vec<_> = m["permissions"]["secrets"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert_eq!(secrets, ["browser.*"], "it opens no other vault item");
    let forms = m["credentials"].as_array().unwrap();
    assert_eq!(forms.len(), 1);
    let form = &forms[0];
    assert!(form["id"].as_str().unwrap().chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    assert!(
        (1..=80).contains(&chars(&form["title"])) && chars(&form["help"]) <= 400,
        "title and help fit core's limits"
    );
    assert_eq!(form["item"].as_str(), Some("browser.{label}"));
    // core's credential item pattern: ^[a-z][a-z0-9_]{1,31}\.([A-Za-z0-9_.-]+|\{label\})$ (manifest.py)
    let item = form["item"].as_str().unwrap();
    let (ext_id, rest) = item.split_once('.').unwrap();
    assert!(
        ext_id.starts_with(|c: char| c.is_ascii_lowercase())
            && (2..=32).contains(&ext_id.len())
            && ext_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && (rest == "{label}" || rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))),
        "item {item} breaks core's pattern"
    );
    assert!(!rest.contains("{label}") || rest == "{label}", "{{label}} is the whole part after the first dot");
    // core's rules for a form with fields (MB-912): no value, placeholder or sign_in; 1-12 fields, unique names
    for k in ["value", "placeholder", "sign_in"] {
        assert!(form.get(k).is_none(), "a form with fields has no {k}");
    }
    let fields = form["fields"].as_array().unwrap();
    assert!((1..=12).contains(&fields.len()));
    let names: Vec<_> = fields.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, browser::login::KEYS, "the fields are the keys the code reads");
    for f in fields {
        let name = f["name"].as_str().unwrap();
        assert!(name.starts_with(|c: char| c.is_ascii_uppercase()) && name.len() <= 64, "{name}");
        assert!(name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "{name}");
        assert!((1..=80).contains(&chars(&f["title"])), "{name}'s title");
        assert!(f.get("placeholder").is_none_or(|p| chars(p) <= 80));
    }
    let flag = |n: &str, k: &str| {
        let f = fields.iter().find(|f| f["name"].as_str() == Some(n)).unwrap();
        f.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
    };
    assert!(flag("URL", "plain") && flag("EMAIL", "plain") && flag("ACCESS_CLIENT_ID", "plain"), "shown as typed");
    assert!(!flag("PASSWORD", "plain") && !flag("ACCESS_CLIENT_SECRET", "plain"), "hidden");
    assert!(flag("ACCESS_CLIENT_ID", "optional") && flag("ACCESS_CLIENT_SECRET", "optional"));
    assert!(!flag("URL", "optional") && !flag("EMAIL", "optional") && !flag("PASSWORD", "optional"));
    let add = m["commands"].as_array().unwrap().iter().find(|c| c["slash"].as_str() == Some("add")).unwrap();
    assert_eq!(add["form"].as_str(), form["id"].as_str(), "`/browser add` opens the form");
    let skill = m["behavior"]["skill"].as_str().unwrap();
    let guidance = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/", "SKILLS.md")).unwrap();
    assert_eq!(skill, "SKILLS.md");
    assert!(guidance.starts_with("---\n"), "the skill's frontmatter first");
    assert!(guidance.contains("your own session"), "it tells agents they have a session of their own");
    assert!(guidance.contains("test account"), "and to use test accounts, never a person's browser");
}

/// The release packs the skill beside the binary, so a node has the guidance from the build alone.
#[test]
fn the_release_packs_the_skill() {
    let release =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.github/workflows/release.yml")).unwrap();
    assert!(release.contains("cp extension.toml SKILLS.md README.md LICENSE pkg/"), "{release}");
    assert!(release.contains("-C pkg extension.toml SKILLS.md bin README.md LICENSE"), "{release}");
}
