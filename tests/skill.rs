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

const SEND_BACK_ONLY_BLOCKING: [&str; 8] = [
    "A finding is blocking when it's inside the task's acceptance criteria, or a correctness or safety bug the PR introduces",
    "Only blocking findings fail a round, and only they go back to the member while it fails",
    "never hold the merge",
    "one follow-up task per PR",
    "when the member has a next PR in the same card's stack, into that PR's scope",
    "Nits go on the PR as a comment marked non-blocking and are not tasks",
    "is each previous blocking finding fixed, and does the fix diff introduce a new correctness or safety bug?",
    "never fails the round",
];

#[test]
fn verify_sends_back_only_blocking_findings_narrows_later_rounds_and_caps_rounds() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let verify = flat_section(&printed, "4. **Verify**", "5. **After merge**");
    for rule in SEND_BACK_ONLY_BLOCKING.into_iter().chain([
        "the diff since that round's head, not the whole PR",
        "After round 3, start no other round",
        "merge as-is, narrow the scope, or change approach",
        "the open blocking findings",
    ]) {
        assert!(verify.contains(rule), "Verify misses {rule:?}");
    }
    assert!(!verify.contains("fits within the task's scope"), "{verify}");
    let lead = flat_section(&printed, "\n## Lead\n", "\n## ");
    for rule in SEND_BACK_ONLY_BLOCKING.into_iter().chain([
        "**Review rounds.**",
        "After your herd's review-round limit (in your prompt; 3 when unset) start no other round",
        "`loopherd report --kind question`",
    ]) {
        assert!(lead.contains(rule), "Lead misses {rule:?}");
    }
    assert!(!lead.contains("other in-scope findings go back"), "{lead}");
    assert!(!lead.contains("After round 3"), "{lead}");
}

#[test]
fn skill_section_prints_only_that_section() {
    let home = tempfile::tempdir().unwrap();
    let printed =
        String::from_utf8(loopherd(home.path(), &["--skill", "--section", "lead"]).stdout).unwrap();
    assert!(printed.starts_with("## Lead\n"), "{printed}");
    assert!(printed.contains("- **On each wake**"), "{printed}");
    assert!(!printed.contains("## Shepherd"), "{printed}");
    assert!(!printed.contains("name: loopherd"), "{printed}");
    assert!(printed.ends_with("never commit in it.\n"), "{printed}");
}

#[test]
fn skill_section_runs_to_the_next_section_past_fenced_headings() {
    let home = tempfile::tempdir().unwrap();
    let printed =
        String::from_utf8(loopherd(home.path(), &["--skill", "--section", "Shepherd"]).stdout)
            .unwrap();
    assert!(printed.starts_with("## Shepherd\n"), "{printed}");
    assert!(printed.contains("\n## State\n"), "{printed}");
    assert!(printed.contains("\n### Talking to members\n"), "{printed}");
    assert!(!printed.contains("\n## Lead\n"), "{printed}");
}

#[test]
fn skill_section_names_the_sections_when_it_knows_none_by_that_name() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["--skill", "--section", "nope"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no section `nope`"), "{err}");
    assert!(err.contains("shepherd") && err.contains("lead"), "{err}");
}

#[test]
fn section_needs_skill() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["--section", "lead"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--skill"), "{err}");
}

#[test]
fn the_shepherd_runs_doctor_at_session_start_and_relays_failures() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let start = flat_section(&printed, "\n## Shepherd\n", "\n### Setting up a project\n");
    for rule in [
        "At the start of each session, run `loopherd doctor`",
        "relay each failure to the human with the fix it names",
    ] {
        assert!(start.contains(rule), "{start}");
    }
}

#[test]
fn only_the_humans_shepherd_names_its_herdr_agent_at_session_start() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let start = flat_section(&printed, "\n## Shepherd\n", "\n### Setting up a project\n");
    let (_, naming) = start
        .split_once("The human's shepherd only, never a lead or member: ")
        .expect("the rename is guarded to the human's shepherd");
    for rule in [
        "`herdr agent rename \"$HERDR_PANE_ID\" shepherd`",
        "If herdr answers `agent_name_taken`, another shepherd already runs",
        "On any other error, carry on and tell the human that `S` can't find you",
    ] {
        assert!(naming.contains(rule), "{naming}");
    }
    let flat = printed.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(flat.matches("herdr agent rename").count(), 1, "{flat}");
}

#[test]
fn only_a_forced_restart_is_the_humans_call() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let handoffs = flat_section(&printed, "\n### Handoffs\n", "\n### Your own handoff\n");
    assert!(
        handoffs.contains(
            "Forcing is the human's call (a lead's PreToolUse hook denies it); \
             a plain restart is not, so run it as soon as the `finished` line arrives."
        ),
        "{handoffs}"
    );
    assert!(!handoffs.contains("It's the human's call"), "{handoffs}");
    let own = flat_section(
        &printed,
        "\n### Your own handoff\n",
        "\n### Notifying the human\n",
    );
    assert!(
        own.contains(
            "First restart any member whose `handoff` and `finished` lines you've already read \
             and that is yours to restart, so the herd doesn't wait on the next shepherd."
        ),
        "{own}"
    );
}

#[test]
fn a_lead_restarts_itself_after_its_handoff() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let lead = flat_section(&printed, "\n## Lead\n", "\n### ");
    assert!(
        lead.contains(
            "Then report `--kind handoff`, stop your watch, run `loopherd restart --self`, \
             and end your turn"
        ),
        "{lead}"
    );
    assert!(!lead.contains("The shepherd restarts you"), "{lead}");
    let whole = flat_section(&printed, "\n## Lead\n", "\n## NO SUCH SECTION\n");
    assert!(
        whole.contains("report `--kind handoff`, run `loopherd restart --self`, and end your turn"),
        "{whole}"
    );
    assert!(!whole.contains("The shepherd restarts you"), "{whole}");
    let handoffs = flat_section(&printed, "\n### Handoffs\n", "\n### Your own handoff\n");
    assert!(
        handoffs.contains("**A lead restarts itself.**"),
        "{handoffs}"
    );
}

#[test]
fn the_shepherd_tells_the_human_to_clear_and_send_any_message_with_the_paste_line_as_fallback() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let own = flat_section(
        &printed,
        "\n### Your own handoff\n",
        "\n### Notifying the human\n",
    );
    let clear = own.find("Run `/clear`, then send any message (e.g. `go`)");
    let paste = own.find("`continue as the loopherd shepherd from ");
    assert!(clear.is_some() && clear < paste, "{own}");
    assert!(
        own.contains("given that resume (naming the `.prev.md` path) or the pasted line"),
        "{own}"
    );
}

#[test]
fn with_self_restart_on_the_shepherd_arms_its_own_restart_and_warns_the_human() {
    let home = tempfile::tempdir().unwrap();
    let printed = String::from_utf8(skill(home.path())).unwrap();
    let own = flat_section(
        &printed,
        "\n### Your own handoff\n",
        "\n### Notifying the human\n",
    );
    for part in [
        "With `[shepherd] self_restart = true`",
        "first run `loopherd restart --self` in this turn",
        "you restart yourself in this pane in 5 minutes, and any prompt here stops that",
        "When it refuses, give the human its reason",
        "`self_restart` notification",
        "ask whether to re-arm",
    ] {
        assert!(own.contains(part), "{part}: {own}");
    }
}
