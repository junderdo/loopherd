//! `loopherd --skill` and the skill `install-hook` installs, through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn loopherd(home: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    out
}

fn skill(home: &Path) -> Vec<u8> {
    loopherd(home, &["--skill"]).stdout
}

#[test]
fn skill_prints_the_skill_md() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    assert!(printed.starts_with("---\nname: loopherd\n"), "{printed}");
    assert!(printed.contains("\n## Shepherd\n"));
    assert!(printed.contains("\n## Lead\n"));
}

#[test]
fn install_hook_writes_the_skill_to_skills_dir_and_remove_deletes_it() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join("settings.json");
    let skills = home.path().join("skills");
    let args = |extra: &[&'static str]| {
        let mut args = vec![
            "install-hook",
            "--settings",
            settings.to_str().unwrap(),
            "--skills-dir",
            skills.to_str().unwrap(),
        ];
        args.extend(extra);
        args
    };
    let installed = skills.join("loopherd/SKILL.md");

    loopherd(home.path(), &args(&[]));
    assert_eq!(fs::read(&installed).unwrap(), skill(home.path()));
    let again = loopherd(home.path(), &args(&[]));
    assert_eq!(fs::read(&installed).unwrap(), skill(home.path()));
    assert!(
        String::from_utf8_lossy(&again.stdout)
            .contains(&format!("{} already up to date", installed.display())),
        "{again:?}"
    );

    loopherd(home.path(), &args(&["--remove"]));
    assert!(!installed.exists());
    assert!(!home.path().join(".claude").exists());
}

#[test]
fn the_skill_defaults_to_the_skills_dir_beside_the_settings_file() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join("claude/settings.json");
    let settings = settings.to_str().unwrap();
    loopherd(home.path(), &["install-hook", "--settings", settings]);
    let installed = home.path().join("claude/skills/loopherd/SKILL.md");
    assert_eq!(fs::read(&installed).unwrap(), skill(home.path()));
    assert!(!home.path().join(".claude").exists());
}

#[test]
fn install_hook_writes_to_claudes_home_skills_dir_by_default() {
    let home = tempfile::tempdir().unwrap();
    loopherd(home.path(), &["install-hook"]);
    let installed = home.path().join(".claude/skills/loopherd/SKILL.md");
    assert_eq!(fs::read(&installed).unwrap(), skill(home.path()));
    loopherd(home.path(), &["install-hook", "--remove"]);
    assert!(!installed.exists());
}

#[test]
fn skill_refuses_a_subcommand_beside_it() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["--skill", "status"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn the_skill_asks_for_a_reviewer_by_role() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let flat = printed.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "the `reviewer` agent type, else `loopherd-reviewer`, else a general-purpose subagent"
        ),
        "{flat}"
    );
}

#[test]
fn the_skill_tells_the_shepherd_to_edit_herd_files_with_herd_set() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let flat = printed.split_whitespace().collect::<Vec<_>>().join(" ");
    for usage in [
        "`loopherd herd get <herd> <key>`",
        "`loopherd herd set <herd> <key> <value>`",
        "`loopherd herd unset <herd> <key>`",
        "`loopherd herd remove <herd>`",
        "The hook always denies `loopherd herd set`, `unset` and `remove` in your pane",
        "`[lead_defaults]` in `$LOOPHERD_CONFIG_DIR/config.toml`",
        "`loopherd lead-defaults --set`, `loopherd config set` and `unset`, `loopherd setup`, and `scripts/install --lead-defaults` or `--context`",
    ] {
        assert!(flat.contains(usage), "{usage}");
    }
}

fn flat_section(printed: &str, start: &str, end: &str) -> String {
    let (_, rest) = printed.split_once(start).expect("section start");
    let body = rest.split_once(end).map_or(rest, |(body, _)| body);
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn verify_blocks_on_in_scope_findings_rereviews_the_delta_and_caps_rounds() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let verify = flat_section(&printed, "4. **Verify**", "5. **After merge**");
    for rule in [
        "A finding is blocking when it's inside the task's acceptance criteria, or a correctness or safety bug the PR introduces",
        "any other finding that should be fixed and fits within the task's scope",
        "a PR comment marked non-blocking",
        "the PR doesn't wait on them",
        "the diff since that round's head, not the whole PR",
        "After round 3, start no other round",
        "merge as-is, narrow the scope, or change approach",
        "the open blocking findings",
    ] {
        assert!(verify.contains(rule), "Verify misses {rule:?}");
    }
    let lead = flat_section(&printed, "\n## Lead\n", "\n## ");
    for rule in [
        "**Review rounds.**",
        "After round 3",
        "`loopherd report --kind question`",
    ] {
        assert!(lead.contains(rule), "Lead misses {rule:?}");
    }
}
