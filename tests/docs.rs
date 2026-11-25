//! `loopherd docs` and the references `install-hook` writes beside the skill.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const TOPICS: [&str; 6] = [
    "tour",
    "setup",
    "install",
    "glossary",
    "herd-file",
    "runbook",
];

fn loopherd(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn shipped() -> Vec<String> {
    let mut names: Vec<String> = TOPICS.iter().map(|t| format!("{t}.md")).collect();
    names.sort();
    names
}

#[test]
fn docs_without_a_topic_lists_each_topic_with_when_to_read_it() {
    let home = tempfile::tempdir().unwrap();
    let listed = stdout(&loopherd(home.path(), &["docs"]));
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), TOPICS.len(), "{listed}");
    for (line, topic) in lines.iter().zip(TOPICS) {
        let (name, summary) = line.split_once("  ").unwrap();
        assert_eq!(name.trim(), topic, "{listed}");
        assert!(!summary.trim().is_empty(), "{listed}");
    }
    assert!(listed.contains("every command and its flags"), "{listed}");
}

#[test]
fn docs_prints_each_topic_as_install_hook_writes_it() {
    let home = tempfile::tempdir().unwrap();
    let skills = home.path().join("skills");
    let settings = home.path().join("settings.json");
    stdout(&loopherd(
        home.path(),
        &[
            "install-hook",
            "--settings",
            settings.to_str().unwrap(),
            "--skills-dir",
            skills.to_str().unwrap(),
        ],
    ));
    let references = skills.join("loopherd/references");
    assert_eq!(files(&references), shipped());
    for topic in TOPICS {
        let printed = stdout(&loopherd(home.path(), &["docs", topic]));
        assert!(printed.starts_with("# "), "{topic}: {printed}");
        let written = fs::read_to_string(references.join(format!("{topic}.md"))).unwrap();
        assert_eq!(printed, written, "{topic}");
    }
}

#[test]
fn docs_with_an_unknown_topic_names_the_topics_and_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let out = loopherd(home.path(), &["docs", "nope"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no topic `nope`"), "{err}");
    assert!(err.contains(&TOPICS.join(", ")), "{err}");
}

#[test]
fn install_hook_writes_references_beside_the_default_skill_and_remove_deletes_them() {
    let home = tempfile::tempdir().unwrap();
    stdout(&loopherd(home.path(), &["install-hook"]));
    let references = home.path().join(".agents/skills/loopherd/references");
    assert_eq!(files(&references), shipped());
    let linked = home.path().join(".claude/skills/loopherd/references");
    assert_eq!(files(&linked), shipped());

    stdout(&loopherd(home.path(), &["install-hook", "--remove"]));
    assert!(!references.exists());
    assert!(!home.path().join(".agents/skills/loopherd").exists());
}

#[test]
fn install_hook_beside_a_settings_file_writes_references_and_reruns_change_nothing() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("test-claude");
    fs::create_dir(&dir).unwrap();
    let settings = dir.join("settings.json");
    let install = ["install-hook", "--settings", settings.to_str().unwrap()];
    stdout(&loopherd(home.path(), &install));
    let references = dir.join(".agents/skills/loopherd/references");
    assert_eq!(files(&references), shipped());
    fs::write(references.join("retired.md"), "no longer shipped").unwrap();
    fs::write(references.join("tour.md"), "an older tour").unwrap();

    let rerun = stdout(&loopherd(home.path(), &install));
    assert!(rerun.contains("installed"), "{rerun}");
    assert_eq!(files(&references), shipped());
    let tour = stdout(&loopherd(home.path(), &["docs", "tour"]));
    assert_eq!(
        fs::read_to_string(references.join("tour.md")).unwrap(),
        tour
    );

    let again = stdout(&loopherd(home.path(), &install));
    assert!(again.contains("already up to date"), "{again}");
}
