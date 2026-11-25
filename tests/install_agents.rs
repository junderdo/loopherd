//! `scripts/install`'s optional agent types (D26): `--with-agents`, the
//! question on a terminal, and the per-file hash rules.

mod fakes;

use fakes::{copied, executable};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REPO: &str = env!("CARGO_MANIFEST_DIR");
const AGENTS: [&str; 4] = ["builder", "reviewer", "researcher", "scout"];

/// A copy of the installer and `agents/` under a temp root, so a test can
/// change loopherd's version of an agent, with a temp HOME and state dir and
/// a fake cargo whose "build" ignores its arguments.
struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    path: String,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("agents")).unwrap();
        copied(
            format!("{REPO}/scripts/install-extras"),
            root.join("scripts/install-extras"),
        );
        copied(
            format!("{REPO}/scripts/install-agents.bash"),
            root.join("scripts/install-agents.bash"),
        );
        copied(
            format!("{REPO}/scripts/install-skills.bash"),
            root.join("scripts/install-skills.bash"),
        );
        copied(
            format!("{REPO}/scripts/install-herdr-plugin.bash"),
            root.join("scripts/install-herdr-plugin.bash"),
        );
        copied(
            format!("{REPO}/scripts/install-herdr-keys.bash"),
            root.join("scripts/install-herdr-keys.bash"),
        );
        executable(
            &root.join("scripts/install"),
            fs::read_to_string(format!("{REPO}/scripts/install")).unwrap(),
        );
        for name in AGENTS {
            fs::copy(
                format!("{REPO}/agents/{name}.md"),
                root.join(format!("agents/{name}.md")),
            )
            .unwrap();
        }
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        let built = dir.path().join("built-loopherd");
        // Does nothing, setup included, so a run on a terminal asks nothing about config.toml.
        executable(&built, "#!/bin/sh\n");
        // Stands in for herdr, so no run can reach the human's plugin registry.
        executable(&fake_bin.join("herdr"), "#!/bin/sh\nexit 1\n");
        executable(
            &fake_bin.join("cargo"),
            format!(
                "#!/bin/sh\necho '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"loopherd\"}},\"executable\":\"{}\"}}'\n",
                built.display()
            ),
        );
        // Reports the plugin installed, so a run never reaches a real claude or the network.
        executable(
            &fake_bin.join("claude"),
            "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"user\"}]'\n",
        );
        let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
        Sandbox {
            _dir: dir,
            root,
            home,
            state,
            path,
        }
    }

    fn agents_dir(&self) -> PathBuf {
        self.home.join(".claude/agents")
    }

    fn installed(&self, file: &str) -> PathBuf {
        self.agents_dir().join(file)
    }

    fn ours(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(format!("agents/{name}.md"))).unwrap()
    }

    fn set_ours(&self, name: &str, text: &str) {
        fs::write(self.root.join(format!("agents/{name}.md")), text).unwrap();
    }

    fn put(&self, file: &str, text: &str) {
        fs::create_dir_all(self.installed(file).parent().unwrap()).unwrap();
        fs::write(self.installed(file), text).unwrap();
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.installed(file)).unwrap()
    }

    fn agent_files(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.agents_dir()) else {
            return vec![];
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("PATH", &self.path)
            .env("LOOPHERD_CONFIG_DIR", self.home.join(".config/loopherd"))
            .env("LOOPHERD_NO_SPLASH", "1")
            .env("HERDR_SOCKET_PATH", self.home.join("no-herdr.sock"))
            .env_remove("CLAUDE_CONFIG_DIR");
    }

    /// Runs the installer with stdin not a terminal, from `cwd`, with `vars` set.
    fn install_with(&self, args: &[&str], cwd: &Path, vars: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(self.root.join("scripts/install"));
        cmd.args(args).current_dir(cwd).stdin(Stdio::null());
        self.env(&mut cmd);
        for (key, value) in vars {
            cmd.env(key, value);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    }

    /// Runs the installer with stdin not a terminal.
    fn install(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(self.root.join("scripts/install"));
        cmd.args(args).stdin(Stdio::null());
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        out
    }

    /// Runs the installer on a pty, typing `answers`, and returns what it
    /// printed; None where util-linux `script` isn't available.
    fn install_on_terminal(&self, args: &[&str], answers: &str) -> Option<String> {
        let util_linux = Command::new("script")
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("util-linux"));
        if !util_linux {
            eprintln!("skipped: needs util-linux script");
            return None;
        }
        let shell = format!(
            "exec '{}' {}",
            self.root.join("scripts/install").display(),
            args.join(" ")
        );
        let mut cmd = Command::new("script");
        cmd.args(["-qec", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.env(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answers.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        Some(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn all_installed() -> Vec<String> {
    let mut names: Vec<String> = AGENTS.iter().map(|n| format!("{n}.md")).collect();
    names.sort();
    names
}

#[test]
fn the_repo_ships_four_agent_types_with_frontmatter() {
    for name in AGENTS {
        let text = fs::read_to_string(format!("{REPO}/agents/{name}.md")).unwrap();
        let front = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(front, _)| front)
            .unwrap_or_else(|| panic!("{name}.md has no frontmatter"));
        assert!(front.contains(&format!("name: {name}\n")), "{name}");
        assert!(front.contains("description: "), "{name}");
        assert!(front.contains("model: "), "{name}");
        if name != "builder" {
            let tools = front.lines().find(|l| l.starts_with("tools: ")).unwrap();
            assert!(
                !tools.contains("Edit") && !tools.contains("Write"),
                "{name}"
            );
        }
    }
}

#[test]
fn the_reviewer_keeps_gate_output_out_of_its_context() {
    let text = fs::read_to_string(format!("{REPO}/agents/reviewer.md")).unwrap();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("through `tail`"), "{flat}");
    assert!(
        flat.contains("never Read a saved `tool-results/` file whole"),
        "{flat}"
    );
}

#[test]
fn with_agents_installs_each_into_the_claude_agents_dir() {
    let sandbox = Sandbox::new();

    sandbox.install(&["--with-agents"]);

    assert_eq!(sandbox.agent_files(), all_installed());
    for name in AGENTS {
        assert_eq!(sandbox.read(&format!("{name}.md")), sandbox.ours(name));
    }
}

#[test]
fn with_agents_and_settings_installs_beside_the_settings_file() {
    let sandbox = Sandbox::new();
    let settings = sandbox.home.join("elsewhere/settings.json");

    sandbox.install(&["--with-agents", "--settings", settings.to_str().unwrap()]);

    let dir = sandbox.home.join("elsewhere/agents");
    assert_eq!(
        fs::read_to_string(dir.join("scout.md")).unwrap(),
        sandbox.ours("scout")
    );
    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
}

#[test]
fn a_rerun_skips_files_identical_to_loopherds() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);

    let out = sandbox.install(&["--with-agents"]);

    assert!(
        stdout(&out).contains(&format!(
            "{} already up to date",
            sandbox.installed("reviewer.md").display()
        )),
        "{}",
        stdout(&out)
    );
    assert_eq!(sandbox.agent_files(), all_installed());
}

#[test]
fn without_a_terminal_installed_agent_types_are_said_to_be_installed() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);

    let out = sandbox.install(&[]);

    assert!(
        stdout(&out).contains(&format!(
            "loopherd's agent types are already installed in {}; left as is",
            sandbox.agents_dir().display()
        )),
        "{}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("add --with-agents"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn agent_types_found_installed_are_recorded_so_a_later_change_updates_them() {
    let sandbox = Sandbox::new();
    for name in ["builder", "reviewer", "researcher", "scout"] {
        sandbox.put(&format!("{name}.md"), &sandbox.ours(name));
    }
    sandbox.install(&[]);
    sandbox.set_ours("reviewer", "---\nname: reviewer\n---\nnew text\n");

    let out = sandbox.install(&["--with-agents"]);

    assert!(
        stdout(&out).contains(&format!(
            "updated {}",
            sandbox.installed("reviewer.md").display()
        )),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_file_unchanged_since_loopherd_installed_it_is_updated() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    sandbox.set_ours("builder", "---\nname: builder\n---\nnewer\n");

    let out = sandbox.install(&["--with-agents"]);

    assert_eq!(
        sandbox.read("builder.md"),
        "---\nname: builder\n---\nnewer\n"
    );
    assert!(stdout(&out).contains("updated "), "{}", stdout(&out));
    assert_eq!(sandbox.agent_files(), all_installed());
}

#[test]
fn without_a_terminal_a_file_edited_since_install_is_kept_and_said_so() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    sandbox.put("builder.md", "my edit\n");
    sandbox.set_ours("builder", "---\nname: builder\n---\nnewer\n");

    let out = sandbox.install(&["--with-agents"]);

    assert_eq!(sandbox.read("builder.md"), "my edit\n");
    let path = sandbox.installed("builder.md");
    assert!(
        stdout(&out).contains(&format!(
            "kept {} (edited since loopherd installed it)",
            path.display()
        )),
        "{}",
        stdout(&out)
    );
    assert_eq!(sandbox.agent_files(), all_installed());
}

#[test]
fn without_a_terminal_the_humans_own_file_is_kept_and_said_so() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");

    let out = sandbox.install(&["--with-agents"]);

    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    assert!(
        stdout(&out).contains(&format!(
            "kept {} (yours, not loopherd's)",
            sandbox.installed("reviewer.md").display()
        )),
        "{}",
        stdout(&out)
    );
    assert_eq!(sandbox.read("scout.md"), sandbox.ours("scout"));
}

#[test]
fn a_humans_file_that_matches_loopherds_is_taken_as_installed() {
    let sandbox = Sandbox::new();
    sandbox.put("scout.md", &sandbox.ours("scout"));
    sandbox.install(&["--with-agents"]);
    sandbox.set_ours("scout", "---\nname: scout\n---\nnewer\n");

    sandbox.install(&["--with-agents"]);

    assert_eq!(sandbox.read("scout.md"), "---\nname: scout\n---\nnewer\n");
}

#[test]
fn without_a_terminal_or_with_agents_nothing_is_installed() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
    assert!(
        stdout(&out).contains("add --with-agents"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn with_agents_and_no_extras_together_is_an_error() {
    let sandbox = Sandbox::new();
    let mut cmd = Command::new(sandbox.root.join("scripts/install"));
    cmd.args(["--with-agents", "--no-extras"]);
    sandbox.env(&mut cmd);

    let out = cmd.output().unwrap();

    assert_eq!(out.status.code(), Some(2));
    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
}

#[test]
fn no_extras_on_a_terminal_asks_nothing_and_installs_no_agents() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&["--no-extras"], "y\n") else {
        return;
    };

    assert!(!out.contains("Install loopherd's agent types"), "{out}");
    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
}

#[test]
fn on_a_terminal_the_question_defaults_to_no() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&[], "\n") else {
        return;
    };

    assert!(out.contains("Install loopherd's agent types"), "{out}");
    assert!(out.contains("[y/N]"), "{out}");
    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
}

#[test]
fn on_a_terminal_yes_installs_them_after_listing_the_overlaps() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");

    let Some(out) = sandbox.install_on_terminal(&[], "y\nk\n") else {
        return;
    };

    let overlap = out
        .find(&format!(
            "{}: yours, not loopherd's",
            sandbox.installed("reviewer.md").display()
        ))
        .unwrap_or_else(|| panic!("{out}"));
    let question = out.find("Install loopherd's agent types").unwrap();
    assert!(overlap < question, "{out}");
    assert!(!out.contains("builder.md: "), "{out}");
    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    assert_eq!(sandbox.read("builder.md"), sandbox.ours("builder"));
}

#[test]
fn a_conflict_answered_keep_leaves_the_humans_file() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");

    let Some(_) = sandbox.install_on_terminal(&["--with-agents"], "\n") else {
        return;
    };

    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    assert_eq!(sandbox.agent_files(), all_installed());
}

#[test]
fn a_conflict_answered_overwrite_backs_up_the_humans_file_first() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");

    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "o\n") else {
        return;
    };

    assert_eq!(sandbox.read("reviewer.md"), sandbox.ours("reviewer"));
    let backups: Vec<String> = sandbox
        .agent_files()
        .into_iter()
        .filter(|f| f.starts_with("reviewer.md.") && f.ends_with(".bak"))
        .collect();
    assert_eq!(backups.len(), 1, "{:?}", sandbox.agent_files());
    assert_eq!(sandbox.read(&backups[0]), "my reviewer\n");
    assert!(out.contains("backup "), "{out}");
}

#[test]
fn an_overwritten_symlink_is_replaced_and_its_target_left_alone() {
    let sandbox = Sandbox::new();
    let target = sandbox.home.join("dotfiles-reviewer.md");
    fs::write(&target, "my reviewer\n").unwrap();
    fs::create_dir_all(sandbox.agents_dir()).unwrap();
    std::os::unix::fs::symlink(&target, sandbox.installed("reviewer.md")).unwrap();

    let Some(_) = sandbox.install_on_terminal(&["--with-agents"], "o\n") else {
        return;
    };

    assert_eq!(fs::read_to_string(&target).unwrap(), "my reviewer\n");
    assert!(!sandbox.installed("reviewer.md").is_symlink());
    assert_eq!(sandbox.read("reviewer.md"), sandbox.ours("reviewer"));
}

#[test]
fn a_conflict_answered_loopherd_name_installs_beside_it_and_stays_there() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");

    let Some(_) = sandbox.install_on_terminal(&["--with-agents"], "l\n") else {
        return;
    };

    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    let aliased = sandbox.read("loopherd-reviewer.md");
    assert!(aliased.contains("\nname: loopherd-reviewer\n"), "{aliased}");
    assert_eq!(
        aliased.replace("name: loopherd-reviewer", "name: reviewer"),
        sandbox.ours("reviewer")
    );

    let ours = sandbox
        .ours("reviewer")
        .replace("model: opus", "model: sonnet");
    sandbox.set_ours("reviewer", &ours);
    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "") else {
        return;
    };

    assert!(!out.contains("[K/o"), "{out}");
    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    assert!(
        sandbox
            .read("loopherd-reviewer.md")
            .contains("model: sonnet")
    );
}

#[test]
fn an_unreadable_file_is_kept_not_taken_as_loopherds() {
    let sandbox = Sandbox::new();
    sandbox.put("reviewer.md", "my reviewer\n");
    let path = sandbox.installed("reviewer.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&path).is_ok() {
        eprintln!("skipped: running as a user who reads mode-000 files");
        return;
    }

    let out = sandbox.install(&["--with-agents"]);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(sandbox.read("reviewer.md"), "my reviewer\n");
    assert!(stdout(&out).contains("kept "), "{}", stdout(&out));
}

#[test]
fn a_dangling_symlink_is_kept() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.agents_dir()).unwrap();
    let link = sandbox.installed("scout.md");
    std::os::unix::fs::symlink(sandbox.home.join("not-mounted/scout.md"), &link).unwrap();

    let out = sandbox.install(&["--with-agents"]);

    assert!(link.is_symlink());
    assert!(stdout(&out).contains("kept "), "{}", stdout(&out));
}

#[test]
fn a_symlink_to_a_copy_of_what_loopherd_installed_is_not_replaced() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    let dotfiles = sandbox.home.join("dotfiles-reviewer.md");
    fs::rename(sandbox.installed("reviewer.md"), &dotfiles).unwrap();
    std::os::unix::fs::symlink(&dotfiles, sandbox.installed("reviewer.md")).unwrap();
    let before = sandbox.ours("reviewer");
    sandbox.set_ours("reviewer", "---\nname: reviewer\n---\nnewer\n");

    sandbox.install(&["--with-agents"]);

    assert!(sandbox.installed("reviewer.md").is_symlink());
    assert_eq!(fs::read_to_string(&dotfiles).unwrap(), before);
}

#[test]
fn another_file_declaring_the_same_agent_name_is_an_overlap_and_kept() {
    let sandbox = Sandbox::new();
    let mine = "---\nname: reviewer\ndescription: mine\n---\nmy reviewer\n";
    sandbox.put("review/strict.md", mine);

    let out = sandbox.install(&["--with-agents"]);

    let listed = format!(
        "{}: {} already declares name: reviewer",
        sandbox.installed("reviewer.md").display(),
        sandbox.installed("review/strict.md").display()
    );
    assert!(stdout(&out).contains(&listed), "{}", stdout(&out));
    assert!(!sandbox.installed("reviewer.md").exists());
    assert_eq!(sandbox.read("review/strict.md"), mine);
    assert_eq!(sandbox.read("scout.md"), sandbox.ours("scout"));
}

#[test]
fn a_same_name_overlap_answered_loopherd_name_installs_beside_it() {
    let sandbox = Sandbox::new();
    sandbox.put("strict.md", "---\nname: reviewer\n---\nmine\n");

    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "l\n") else {
        return;
    };

    assert!(!out.contains("[o]verwrite"), "{out}");
    assert!(!sandbox.installed("reviewer.md").exists());
    assert!(
        sandbox
            .read("loopherd-reviewer.md")
            .contains("\nname: loopherd-reviewer\n")
    );
}

#[test]
fn agents_go_under_claude_config_dir_when_it_is_set() {
    let sandbox = Sandbox::new();
    let config = sandbox.home.join("config/claude");
    let mut cmd = Command::new(sandbox.root.join("scripts/install"));
    cmd.arg("--with-agents").stdin(Stdio::null());
    sandbox.env(&mut cmd);
    cmd.env("CLAUDE_CONFIG_DIR", &config);

    assert!(cmd.output().unwrap().status.success());

    assert_eq!(
        fs::read_to_string(config.join("agents/builder.md")).unwrap(),
        sandbox.ours("builder")
    );
}

#[test]
fn remove_offers_no_agents() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&["--remove"], "y\n") else {
        return;
    };

    assert!(!out.contains("Install loopherd's agent types"), "{out}");
    assert_eq!(sandbox.agent_files(), Vec::<String>::new());
}

#[test]
fn a_file_installed_under_one_spelling_of_the_dir_is_updated_under_another() {
    let sandbox = Sandbox::new();
    let config = sandbox.home.join("config");
    fs::create_dir_all(&config).unwrap();
    let link = sandbox.home.join("linked-config");
    std::os::unix::fs::symlink(&config, &link).unwrap();
    let trailing = PathBuf::from(format!("{}/", link.display()));
    sandbox.install_with(
        &["--with-agents"],
        &sandbox.home,
        &[("CLAUDE_CONFIG_DIR", &trailing)],
    );
    sandbox.set_ours("builder", "---\nname: builder\n---\nnewer\n");

    let out = sandbox.install_with(
        &["--with-agents", "--settings", "./config/settings.json"],
        &sandbox.home,
        &[],
    );

    assert_eq!(
        fs::read_to_string(config.join("agents/builder.md")).unwrap(),
        "---\nname: builder\n---\nnewer\n"
    );
    assert!(stdout(&out).contains("updated "), "{}", stdout(&out));
}

#[test]
fn the_ledger_goes_under_home_when_xdg_state_home_is_unset() {
    let sandbox = Sandbox::new();
    let mut cmd = Command::new(sandbox.root.join("scripts/install"));
    cmd.arg("--with-agents").stdin(Stdio::null());
    sandbox.env(&mut cmd);
    cmd.env_remove("XDG_STATE_HOME");

    assert!(cmd.output().unwrap().status.success());

    let ledger =
        fs::read_to_string(sandbox.home.join(".local/state/loopherd/agents.sha256")).unwrap();
    assert!(
        ledger.contains(&format!(" {}\n", sandbox.installed("scout.md").display())),
        "{ledger}"
    );
}

#[test]
fn a_target_that_is_not_a_readable_file_is_not_offered_overwrite_and_the_rest_install() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.installed("reviewer.md")).unwrap();

    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "o\n") else {
        return;
    };

    assert!(!out.contains("[o]verwrite"), "{out}");
    assert!(sandbox.installed("reviewer.md").is_dir());
    assert_eq!(sandbox.read("scout.md"), sandbox.ours("scout"));
}

#[test]
fn typing_an_action_word_cannot_pick_an_option_not_offered() {
    let sandbox = Sandbox::new();
    sandbox.put("strict.md", "---\nname: reviewer\n---\nmine\n");

    for typed in ["overwrite\n", "alias\n"] {
        let Some(out) = sandbox.install_on_terminal(&["--with-agents"], typed) else {
            return;
        };

        assert!(!sandbox.installed("reviewer.md").exists(), "{typed}: {out}");
        assert!(
            !sandbox.installed("loopherd-reviewer.md").exists(),
            "{typed}: {out}"
        );
    }
}

#[test]
fn loopherds_own_file_is_left_as_is_when_another_file_declares_its_name() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    sandbox.put("strict.md", "---\nname: reviewer\n---\nmine\n");

    let out = sandbox.install(&["--with-agents"]);

    let said = format!(
        "left loopherd's {} as is: {} already declares name: reviewer",
        sandbox.installed("reviewer.md").display(),
        sandbox.installed("strict.md").display()
    );
    assert!(stdout(&out).contains(&said), "{}", stdout(&out));
    assert!(!stdout(&out).contains("kept "), "{}", stdout(&out));
}

#[test]
fn a_file_edited_since_install_answered_overwrite_is_backed_up_and_replaced() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    sandbox.put("builder.md", "my edit\n");

    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "o\n") else {
        return;
    };

    assert!(out.contains("edited since loopherd installed it"), "{out}");
    assert_eq!(sandbox.read("builder.md"), sandbox.ours("builder"));
    let backup = sandbox
        .agent_files()
        .into_iter()
        .find(|f| f.starts_with("builder.md.") && f.ends_with(".bak"))
        .unwrap();
    assert_eq!(sandbox.read(&backup), "my edit\n");
}

#[test]
fn a_file_edited_since_install_answered_loopherd_name_keeps_the_edit() {
    let sandbox = Sandbox::new();
    sandbox.install(&["--with-agents"]);
    sandbox.put("builder.md", "my edit\n");

    let Some(_) = sandbox.install_on_terminal(&["--with-agents"], "l\n") else {
        return;
    };

    assert_eq!(sandbox.read("builder.md"), "my edit\n");
    assert!(
        sandbox
            .read("loopherd-builder.md")
            .contains("\nname: loopherd-builder\n")
    );
}

#[test]
fn the_reviewer_bounds_its_waits_and_stops_its_shells() {
    let text = fs::read_to_string(format!("{REPO}/agents/reviewer.md")).unwrap();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("Every wait you run has a timeout, and you stop every background shell you started before you return"),
        "{flat}"
    );
}
