//! `loopherd herds` through the binary, with a `herdr` on PATH that records any call.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_herds(config: &Path, bin: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("herds")
        .arg("--settings")
        .arg(config.join("settings.json"))
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"))
        .env("PATH", bin)
        .output()
        .unwrap()
}

fn recording_herdr(bin: &Path) -> std::path::PathBuf {
    let calls = bin.join("calls");
    let shim = bin.join("herdr");
    executable(
        &shim,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", calls.display()),
    );
    calls
}

#[test]
fn herds_lists_herd_files_flags_invalid_ones_and_never_calls_herdr() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let calls = recording_herdr(bin.path());
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    fs::write(
        herds.join("prs.toml"),
        "repo = \"/r\"\n[template.pr]\nparams = [\"number\"]\nprompt = \"go\"\n",
    )
    .unwrap();

    let ok = run_herds(config.path(), bin.path());
    assert_eq!(
        String::from_utf8_lossy(&ok.stdout),
        "prs  repo /r  workspace (current)  paused no  stalled after 30m\n  loopherd add prs pr <number>\n"
    );
    assert_eq!(ok.status.code(), Some(0));

    fs::write(herds.join("broken.toml"), "repo = ").unwrap();
    let invalid = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&invalid.stdout);
    assert!(stdout.starts_with("invalid broken.toml: "), "{stdout}");
    assert!(stdout.contains("loopherd add prs pr <number>"), "{stdout}");
    assert_eq!(invalid.status.code(), Some(1));

    fs::remove_file(herds.join("broken.toml")).unwrap();
    fs::write(
        herds.join("typo.toml"),
        "repo = \"/r\"\n[template.t]\nparams = []\nprompt = \"hi {nmae}\"\n",
    )
    .unwrap();
    let typo = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&typo.stdout);
    assert!(
        stdout.ends_with("invalid typo.toml: template t: prompt: unknown placeholder {nmae}\n"),
        "{stdout}"
    );
    assert_eq!(typo.status.code(), Some(1));

    assert!(!calls.exists(), "herdr was called");
}

#[test]
fn herds_without_a_herds_directory_hints_and_exits_0() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let out = run_herds(config.path(), bin.path());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "no herds: add a herd file at {}/<herd>.toml\n",
            config.path().join("herds").display()
        )
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn herds_shows_when_a_herd_was_paused() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    for herd in ["docs", "prs"] {
        fs::write(herds.join(format!("{herd}.toml")), "repo = \"/r\"\n").unwrap();
    }
    let paused = config.path().join("state/loopherd/paused");
    fs::create_dir_all(&paused).unwrap();
    fs::write(
        paused.join("prs"),
        r#"{"since": "2026-11-05T09:00:00Z", "reason": "freeze"}"#,
    )
    .unwrap();

    let out = run_herds(config.path(), bin.path());

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "docs  repo /r  workspace (current)  paused no  stalled after 30m\n\
         prs  repo /r  workspace (current)  paused since 2026-11-05T09:00:00Z  stalled after 30m\n"
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn herds_warns_of_a_missing_prompt_hook_in_the_settings_it_is_given_until_install_hook_adds_it() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    fs::write(herds.join("prs.toml"), "repo = \"/r\"\n").unwrap();
    fs::write(
        config.path().join("config.toml"),
        "[notify]\nenabled = true\npane_label = true\n",
    )
    .unwrap();
    let settings = config.path().join("settings.json");

    let out = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "prompt hook is not in {}, so a pane label in herd prs stays",
            settings.display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("run scripts/install or loopherd install-hook"),
        "{stdout}"
    );
    assert_eq!(out.status.code(), Some(0));

    let installed = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["install-hook", "--settings"])
        .arg(&settings)
        .output()
        .unwrap();
    assert!(installed.status.success(), "{installed:?}");

    let out = run_herds(config.path(), bin.path());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "prs  repo /r  workspace (current)  paused no  stalled after 30m\n"
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn herds_accepts_the_documented_routed_queue_and_its_groom_template() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    recording_herdr(bin.path());
    let herds = config.path().join("herds");
    fs::create_dir_all(herds.join("docs/agents")).unwrap();
    fs::write(
        herds.join("docs/agents/grill-batch-prompt.md"),
        include_str!("../docs/agents/grill-batch-prompt.md"),
    )
    .unwrap();
    fs::write(
        herds.join("cards.toml"),
        format!(
            "repo = \"/r\"\n[template.card]\nparams = [\"card\", \"type\", \"slug\"]\n\
             name = \"lh-{{card}}\"\nbranch = \"{{type}}/LH-{{card}}-{{slug}}\"\n\
             prompt = \"build {{card}}\"\n{}",
            include_str!("../docs/examples/loopherd-cards-routed.toml")
        ),
    )
    .unwrap();

    let out = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("loopherd add cards groom  (no slot)\n"),
        "{stdout}"
    );
}

#[test]
fn the_documented_groom_prompt_gives_self_contained_batch_lines_and_a_heredoc_report() {
    let example = include_str!("../docs/examples/loopherd-cards-routed.toml");
    let (_, groom) = example.split_once("[template.groom]").unwrap();
    for form in [
        "`scripts/trello label LH-<n> +ready-for-agent`",
        r#"`scripts/trello comment LH-<n> "<what's missing>"`"#,
        "`scripts/trello archive LH-<n>  # <reason>`",
        "loopherd report --kind done \"$(cat <<'EOF'",
    ] {
        assert!(
            groom.contains(form),
            "groom prompt misses {form:?}: {groom}"
        );
    }
    assert!(!groom.contains("curl"), "{groom}");
    assert!(!groom.contains("'<report>'"), "{groom}");
}

#[test]
fn herds_takes_a_percentage_context_limit_and_flags_a_bad_one() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    recording_herdr(bin.path());
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    let herd = |limit: &str| {
        format!(
            "repo = \"/r\"\n[template.pr]\nparams = []\nprompt = \"go\"\ncontext_limit = {limit}\n\
             [lead]\nprompt = \"lead\"\ncontext_limit = {limit}\n"
        )
    };
    fs::write(herds.join("prs.toml"), herd("\"65%\"")).unwrap();
    assert_eq!(run_herds(config.path(), bin.path()).status.code(), Some(0));

    for (limit, says) in [
        ("\"0%\"", "between 1% and 100%"),
        ("\"6.5%\"", "a whole percentage"),
        ("\"lots\"", "250000, 250k, 1.5M or 1_000_000"),
    ] {
        fs::write(herds.join("prs.toml"), herd(limit)).unwrap();
        let out = run_herds(config.path(), bin.path());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with("invalid prs.toml: ") && stdout.contains(says),
            "{limit}: {stdout}"
        );
        assert_eq!(out.status.code(), Some(1));
    }
}
