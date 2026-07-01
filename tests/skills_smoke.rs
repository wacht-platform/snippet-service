// Skills are discovered from a folder of <name>/SKILL.md, metadata is exposed for
// the prompt, and load() returns the body (frontmatter stripped) + bundled files.
#[test]
fn discovers_and_loads_skill() {
    let root = std::env::temp_dir().join(format!("snip_skills_{}", std::process::id()));
    let dir = root.join("changelog").join("scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        root.join("changelog").join("SKILL.md"),
        "---\nname: changelog\ndescription: Generate a changelog from git commits.\n---\n\n# Changelog\nRun git log.\n",
    )
    .unwrap();
    std::fs::write(dir.join("collect.sh"), "git log --oneline -20\n").unwrap();

    let found = snippet::skills::discover_in(&root);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "changelog");

    let (sk, body, files): (_, String, Vec<String>) = snippet::skills::load_in(&root, "changelog").unwrap();
    assert_eq!(sk.name, "changelog");
    assert!(!body.starts_with("---"), "frontmatter should be stripped");
    assert!(body.contains("# Changelog"));
    assert!(files.iter().any(|f| f.contains("collect.sh")));
    assert!(files[0].starts_with('/'), "bundled paths should be absolute");

    // search: a matching query surfaces the skill; a blank query lists all.
    let hits = snippet::skills::search_in(&root, "changelog release notes");
    assert_eq!(hits.first().map(|(n, _)| n.as_str()), Some("changelog"));
    assert_eq!(snippet::skills::search_in(&root, "").len(), 1);

    std::fs::remove_dir_all(&root).ok();
}
