//! `scripts/install`'s optional mattpocock-skills plugin (D26): `--with-skills`,
//! the question on a terminal, and the overlap listing, driven against a stub
//! `claude` and `curl` so no test installs a real plugin.

mod fakes;

use fakes::executable;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REPO: &str = env!("CARGO_MANIFEST_DIR");
const PLUGIN: &str = "mattpocock-skills@claude-plugins-official";

/// A dir of links to everything on the test's PATH except `claude`.
fn path_without_claude(dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    for bin in std::env::var("PATH").unwrap().split(':') {
        let Ok(entries) = fs::read_dir(bin) else {
            continue;
        };
        for entry in entries.flatten() {
            let link = dir.join(entry.file_name());
            if entry.file_name() != "claude" && !link.exists() {
                std::os::unix::fs::symlink(entry.path(), link).ok();
            }
        }
    }
    dir.display().to_string()
}

/// The installer under a temp root with a temp HOME and state dir, a fake
/// cargo, and stubs for `claude` and `curl` that log their arguments and
/// answer from files under `stub/`.
struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    stub: PathBuf,
    fake_bin: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("agents")).unwrap();
        executable(
            &root.join("scripts/install"),
            fs::read_to_string(format!("{REPO}/scripts/install")).unwrap(),
        );
        for file in [
            "install-extras",
            "install-agents.bash",
            "install-skills.bash",
            "install-herdr-plugin.bash",
            "install-herdr-keys.bash",
        ] {
            fs::copy(
                format!("{REPO}/scripts/{file}"),
                root.join(format!("scripts/{file}")),
            )
            .unwrap();
        }
        for name in ["builder", "reviewer", "researcher", "scout"] {
            fs::copy(
                format!("{REPO}/agents/{name}.md"),
                root.join(format!("agents/{name}.md")),
            )
            .unwrap();
        }
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        let stub = dir.path().join("stub");
        let fake_bin = dir.path().join("fake-bin");
        for d in [&home, &stub, &fake_bin] {
            fs::create_dir_all(d).unwrap();
        }
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
        let stub_dir = stub.display();
        executable(
            &fake_bin.join("claude"),
            format!(
                r#"#!/bin/sh
echo "claude $*" >> '{stub_dir}/log'
[ -z "$CLAUDE_CONFIG_DIR" ] || echo "config $CLAUDE_CONFIG_DIR" >> '{stub_dir}/log'
case "$*" in
  "plugin list --json") cat '{stub_dir}/installed.json' 2>/dev/null || echo '[]' ;;
  "plugin marketplace list --json") cat '{stub_dir}/marketplaces.json' 2>/dev/null || echo '[]' ;;
  "plugin marketplace add"*) [ ! -e '{stub_dir}/add-fails' ] || exit 1 ;;
  "plugin install"*) [ ! -e '{stub_dir}/install-fails' ] || exit 1 ;;
esac
"#
            ),
        );
        executable(
            &fake_bin.join("curl"),
            format!(
                "#!/bin/sh\nfor a; do url=$a; done\necho \"curl $url\" >> '{stub_dir}/log'\ncat '{stub_dir}/plugin.json' 2>/dev/null || exit 22\n"
            ),
        );
        let sandbox = Sandbox {
            _dir: dir,
            root,
            home,
            state,
            stub,
            fake_bin,
        };
        sandbox.serve_plugin(&["engineering/tdd", "productivity/grilling", "engineering/pr"]);
        sandbox
    }

    /// The manifest the stub `curl` serves, listing `skills` as the plugin does.
    fn serve_plugin(&self, skills: &[&str]) {
        let paths: Vec<String> = skills.iter().map(|s| format!("\"./skills/{s}\"")).collect();
        fs::write(
            self.stub.join("plugin.json"),
            format!(
                "{{\"name\":\"mattpocock-skills\",\"skills\":[{}]}}",
                paths.join(",")
            ),
        )
        .unwrap();
    }

    /// Makes the stub `curl` serve `json` as the plugin's manifest.
    fn serve_manifest(&self, json: &str) {
        fs::write(self.stub.join("plugin.json"), json).unwrap();
    }

    fn report_installed(&self, json: &str) {
        fs::write(self.stub.join("installed.json"), json).unwrap();
    }

    /// Makes the stub report the marketplace as added, with a catalog pinning
    /// the plugin at `sha`.
    fn add_marketplace(&self, sha: &str) {
        self.add_marketplace_from("https://github.com/mattpocock/skills.git", sha);
    }

    /// Like `add_marketplace`, with the catalog naming the plugin's repo by `url`.
    fn add_marketplace_from(&self, url: &str, sha: &str) {
        let location = self
            .home
            .join(".claude/plugins/marketplaces/claude-plugins-official");
        fs::create_dir_all(location.join(".claude-plugin")).unwrap();
        fs::write(
            location.join(".claude-plugin/marketplace.json"),
            format!(
                "{{\"plugins\":[{{\"name\":\"other\",\"source\":{{\"sha\":\"nope\"}}}},{{\"name\":\"mattpocock-skills\",\"source\":{{\"source\":\"url\",\"url\":\"{url}\",\"sha\":\"{sha}\"}}}}]}}"
            ),
        )
        .unwrap();
        fs::write(
            self.stub.join("marketplaces.json"),
            format!(
                "[{{\"name\":\"claude-plugins-official\",\"installLocation\":\"{}\"}}]",
                location.display()
            ),
        )
        .unwrap();
    }

    fn skill(&self, name: &str) {
        fs::create_dir_all(self.home.join(".claude/skills").join(name)).unwrap();
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.stub.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn claude_calls(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| l.starts_with("claude "))
            .collect()
    }

    fn path(&self) -> String {
        format!(
            "{}:{}",
            self.fake_bin.display(),
            std::env::var("PATH").unwrap()
        )
    }

    fn command(&self, program: &Path, path: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", self.home.join(".config/loopherd"))
            .env("LOOPHERD_NO_SPLASH", "1")
            .env("HERDR_SOCKET_PATH", self.home.join("no-herdr.sock"))
            .env_remove("CLAUDE_CONFIG_DIR");
        cmd
    }

    /// Runs the installer with stdin not a terminal.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.command(&self.root.join("scripts/install"), &self.path());
        cmd.args(args).stdin(Stdio::null()).output().unwrap()
    }

    fn install(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
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
        let mut cmd = self.command(Path::new("script"), &self.path());
        cmd.args(["-qec", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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

#[test]
fn with_skills_adds_the_marketplace_first_when_it_is_missing() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&["--with-skills"]);

    let calls = sandbox.claude_calls();
    let add = calls
        .iter()
        .position(|c| c == "claude plugin marketplace add anthropics/claude-plugins-official")
        .unwrap_or_else(|| panic!("{calls:?}"));
    assert_eq!(
        calls[add + 1..],
        [format!("claude plugin install {PLUGIN}")]
    );
    assert!(
        out.contains("installed the mattpocock-skills plugin"),
        "{out}"
    );
}

#[test]
fn with_skills_and_the_marketplace_present_installs_without_adding_it() {
    let sandbox = Sandbox::new();
    sandbox.add_marketplace("abc123");

    sandbox.install(&["--with-skills"]);

    let calls = sandbox.claude_calls();
    assert!(
        !calls.iter().any(|c| c.contains("marketplace add")),
        "{calls:?}"
    );
    assert_eq!(
        calls.last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn the_skill_names_are_read_from_the_plugin_at_the_commit_the_catalog_pins() {
    let sandbox = Sandbox::new();
    sandbox.add_marketplace("abc123");

    sandbox.install(&["--with-skills"]);

    assert!(
        sandbox.log().contains(
            &"curl https://raw.githubusercontent.com/mattpocock/skills/abc123/.claude-plugin/plugin.json"
                .to_string()
        ),
        "{:?}",
        sandbox.log()
    );
}

#[test]
fn without_the_marketplace_the_skill_names_are_read_at_the_plugins_head() {
    let sandbox = Sandbox::new();

    sandbox.install(&["--with-skills"]);

    assert!(
        sandbox.log().contains(
            &"curl https://raw.githubusercontent.com/mattpocock/skills/HEAD/.claude-plugin/plugin.json"
                .to_string()
        ),
        "{:?}",
        sandbox.log()
    );
}

#[test]
fn a_failed_marketplace_add_fails_the_installer_without_installing() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("add-fails"), "").unwrap();

    let out = sandbox.run(&["--with-skills"]);

    assert!(!out.status.success(), "{out:?}");
    assert!(
        !sandbox
            .claude_calls()
            .iter()
            .any(|c| c.starts_with("claude plugin install")),
        "{:?}",
        sandbox.claude_calls()
    );
}

#[test]
fn with_settings_claude_runs_with_the_config_dir_beside_it() {
    let sandbox = Sandbox::new();
    let settings = sandbox.home.join("elsewhere/settings.json");

    sandbox.install(&["--with-skills", "--settings", settings.to_str().unwrap()]);

    let dirs: Vec<String> = sandbox
        .log()
        .into_iter()
        .filter(|l| l.starts_with("config "))
        .collect();
    assert!(!dirs.is_empty());
    let want = format!("config {}", sandbox.home.join("elsewhere").display());
    assert!(dirs.iter().all(|d| *d == want), "{dirs:?}");
}

#[test]
fn an_installed_plugin_is_reported_and_left_alone() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.stub.join("installed.json"),
        format!("[{{\"id\":\"other@x\"}},{{\"id\":\"{PLUGIN}\",\"version\":\"1.3.1\",\"scope\":\"user\"}}]"),
    )
    .unwrap();

    let out = sandbox.install(&["--with-skills"]);

    assert_eq!(sandbox.claude_calls(), ["claude plugin list --json"]);
    assert!(
        out.contains("mattpocock-skills plugin is already installed"),
        "{out}"
    );
}

#[test]
fn with_skills_lists_the_humans_skills_that_share_a_name_with_the_plugins() {
    let sandbox = Sandbox::new();
    sandbox.skill("tdd");
    sandbox.skill("mine");
    let skills = sandbox.home.join(".claude/skills");
    std::os::unix::fs::symlink("/nowhere", skills.join("grilling")).unwrap();

    let out = sandbox.install(&["--with-skills"]);

    assert!(
        out.contains(&format!("  {}\n", skills.join("tdd").display())),
        "{out}"
    );
    assert!(
        out.contains(&format!("  {}\n", skills.join("grilling").display())),
        "{out}"
    );
    assert!(!out.contains("/mine"), "{out}");
    assert!(!out.contains("/pr\n"), "{out}");
    assert!(out.contains("mattpocock-skills:<name>"), "{out}");
    assert!(skills.join("tdd").is_dir());
}

#[test]
fn no_overlap_lists_nothing() {
    let sandbox = Sandbox::new();
    sandbox.skill("mine");

    let out = sandbox.install(&["--with-skills"]);

    assert!(!out.contains("both will load"), "{out}");
}

#[test]
fn an_unreadable_plugin_manifest_is_said_and_the_install_goes_on() {
    let sandbox = Sandbox::new();
    fs::remove_file(sandbox.stub.join("plugin.json")).unwrap();

    let out = sandbox.install(&["--with-skills"]);

    assert!(
        out.contains("could not read the mattpocock-skills plugin's skill names"),
        "{out}"
    );
    assert_eq!(
        sandbox.claude_calls().last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn on_a_terminal_the_overlaps_come_before_the_question_and_yes_installs() {
    let sandbox = Sandbox::new();
    sandbox.skill("tdd");

    let Some(out) = sandbox.install_on_terminal(&[], "\ny\n") else {
        return;
    };

    let overlap = out
        .find(&format!(
            "  {}",
            sandbox.home.join(".claude/skills/tdd").display()
        ))
        .unwrap_or_else(|| panic!("{out}"));
    let question = out
        .find("Install the mattpocock-skills plugin")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(overlap < question, "{out}");
    assert_eq!(
        sandbox.claude_calls().last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn on_a_terminal_the_question_defaults_to_no() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&[], "\n\n") else {
        return;
    };

    assert!(
        out.contains("Install the mattpocock-skills plugin"),
        "{out}"
    );
    assert!(out.contains("[y/N]"), "{out}");
    assert!(
        !sandbox
            .claude_calls()
            .iter()
            .any(|c| c.contains("install") || c.contains("add")),
        "{:?}",
        sandbox.claude_calls()
    );
}

#[test]
fn no_extras_on_a_terminal_asks_nothing_and_calls_no_claude() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&["--no-extras"], "y\ny\n") else {
        return;
    };

    assert!(!out.contains("mattpocock-skills"), "{out}");
    assert_eq!(sandbox.claude_calls(), Vec::<String>::new());
}

#[test]
fn without_a_terminal_it_is_skipped_and_says_how_to_install_it() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert!(out.contains("add --with-skills"), "{out}");
    assert_eq!(
        sandbox.claude_calls(),
        vec!["claude plugin list --json".to_string()]
    );
}

#[test]
fn a_missing_claude_cli_skips_it_with_a_message() {
    let sandbox = Sandbox::new();
    fs::remove_file(sandbox.fake_bin.join("claude")).unwrap();
    let path = format!(
        "{}:{}",
        sandbox.fake_bin.display(),
        path_without_claude(&sandbox.stub.join("path"))
    );
    let mut cmd = sandbox.command(&sandbox.root.join("scripts/install"), &path);

    let out = cmd
        .arg("--with-skills")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no claude CLI"),
        "{out:?}"
    );
}

#[test]
fn with_skills_and_no_extras_together_is_an_error() {
    let sandbox = Sandbox::new();

    let out = sandbox.run(&["--with-skills", "--no-extras"]);

    assert_eq!(out.status.code(), Some(2));
    assert_eq!(sandbox.claude_calls(), Vec::<String>::new());
}

#[test]
fn naming_one_extra_installs_only_that_one_without_questions() {
    let sandbox = Sandbox::new();
    let Some(out) = sandbox.install_on_terminal(&["--with-skills"], "y\n") else {
        return;
    };

    assert!(!out.contains("agent types"), "{out}");
    assert!(
        !out.contains("Install the mattpocock-skills plugin"),
        "{out}"
    );
    assert!(!sandbox.home.join(".claude/agents").exists());
    assert_eq!(
        sandbox.claude_calls().last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn with_agents_alone_calls_no_claude() {
    let sandbox = Sandbox::new();

    sandbox.install(&["--with-agents"]);

    assert_eq!(sandbox.claude_calls(), Vec::<String>::new());
}

#[test]
fn both_extras_can_be_named_together() {
    let sandbox = Sandbox::new();

    sandbox.install(&["--with-agents", "--with-skills"]);

    assert_eq!(
        sandbox.claude_calls().last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn a_failed_plugin_install_fails_the_installer() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("install-fails"), "").unwrap();

    let out = sandbox.run(&["--with-skills"]);

    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("claude plugin install"),
        "{out:?}"
    );
}

#[test]
fn overlaps_are_read_from_the_skills_dir_beside_the_settings_file() {
    let sandbox = Sandbox::new();
    let settings = sandbox.home.join("elsewhere/settings.json");
    fs::create_dir_all(sandbox.home.join("elsewhere/skills/tdd")).unwrap();
    sandbox.skill("grilling");

    let out = sandbox.install(&["--with-skills", "--settings", settings.to_str().unwrap()]);

    assert!(out.contains("elsewhere/skills/tdd"), "{out}");
    assert!(!out.contains(".claude/skills/grilling"), "{out}");
}

#[test]
fn remove_offers_no_skills() {
    let sandbox = Sandbox::new();

    sandbox.install(&["--remove", "--with-skills"]);

    assert_eq!(sandbox.claude_calls(), Vec::<String>::new());
}

#[test]
fn an_install_at_project_or_local_scope_does_not_count_as_installed() {
    let sandbox = Sandbox::new();
    sandbox.report_installed(&format!(
        "[{{\"id\":\"{PLUGIN}\",\"scope\":\"project\"}},{{\"id\":\"{PLUGIN}\",\"scope\":\"local\"}}]"
    ));

    let out = sandbox.install(&["--with-skills"]);

    assert!(!out.contains("already installed"), "{out}");
    assert_eq!(
        sandbox.claude_calls().last().unwrap(),
        &format!("claude plugin install {PLUGIN}")
    );
}

#[test]
fn a_managed_install_counts_as_installed() {
    let sandbox = Sandbox::new();
    sandbox.report_installed(&format!("[{{\"id\":\"{PLUGIN}\",\"scope\":\"managed\"}}]"));

    let out = sandbox.install(&["--with-skills"]);

    assert!(out.contains("already installed"), "{out}");
    assert_eq!(sandbox.claude_calls(), ["claude plugin list --json"]);
}

#[test]
fn with_agents_alone_on_a_terminal_never_asks_about_the_plugin() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&["--with-agents"], "y\ny\n") else {
        return;
    };

    assert!(!out.contains("mattpocock-skills"), "{out}");
    assert_eq!(sandbox.claude_calls(), Vec::<String>::new());
}

#[test]
fn a_manifest_whose_skills_is_one_path_names_that_skill() {
    let sandbox = Sandbox::new();
    sandbox.serve_manifest(r#"{"skills":"./skills/engineering/tdd/"}"#);
    sandbox.skill("tdd");

    let out = sandbox.install(&["--with-skills"]);

    let tdd = sandbox.home.join(".claude/skills/tdd");
    assert!(out.contains(&format!("  {}\n", tdd.display())), "{out}");
}

#[test]
fn a_manifest_without_skills_is_said_to_be_unreadable() {
    let sandbox = Sandbox::new();
    sandbox.serve_manifest(r#"{"name":"mattpocock-skills"}"#);
    sandbox.skill("tdd");

    let out = sandbox.install(&["--with-skills"]);

    assert!(
        out.contains("could not read the mattpocock-skills plugin's skill names"),
        "{out}"
    );
    assert!(!out.contains("both will load"), "{out}");
}

#[test]
fn a_skills_path_of_dot_slash_is_not_a_skill_named_dot() {
    let sandbox = Sandbox::new();
    sandbox.serve_manifest(r#"{"skills":["./", "./skills/engineering/pr"]}"#);
    sandbox.skill("mine");

    let out = sandbox.install(&["--with-skills"]);

    assert!(!out.contains("both will load"), "{out}");
}

#[test]
fn the_skill_names_are_read_from_the_repo_the_catalog_names() {
    let sandbox = Sandbox::new();
    sandbox.add_marketplace_from("https://github.com/someone/fork.git", "f00d");

    sandbox.install(&["--with-skills"]);

    assert!(
        sandbox.log().contains(
            &"curl https://raw.githubusercontent.com/someone/fork/f00d/.claude-plugin/plugin.json"
                .to_string()
        ),
        "{:?}",
        sandbox.log()
    );
}

#[test]
fn a_catalog_naming_no_github_repo_is_said_to_be_unreadable() {
    let sandbox = Sandbox::new();
    sandbox.add_marketplace_from("https://example.com/skills.git", "f00d");

    let out = sandbox.install(&["--with-skills"]);

    assert!(
        out.contains("could not read the mattpocock-skills plugin's skill names"),
        "{out}"
    );
    assert!(
        !sandbox.log().iter().any(|l| l.starts_with("curl ")),
        "{:?}",
        sandbox.log()
    );
}

#[test]
fn with_settings_the_config_dir_is_named_in_what_it_prints() {
    let sandbox = Sandbox::new();
    let dir = sandbox.home.join("proj/.claude");
    sandbox.serve_manifest(r#"{"skills":["./skills/engineering/tdd"]}"#);

    let Some(out) = sandbox.install_on_terminal(
        &[
            "--settings",
            dir.join("settings.local.json").to_str().unwrap(),
        ],
        "\ny\n",
    ) else {
        return;
    };

    let named = format!(" in {}", dir.display());
    assert!(
        out.contains(&format!(
            "mattpocock-skills@claude-plugins-official){named}? [y/N]"
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("installed the mattpocock-skills plugin{named}\n")),
        "{out}"
    );
}

#[test]
fn with_settings_an_installed_plugin_names_the_config_dir() {
    let sandbox = Sandbox::new();
    let dir = sandbox.home.join("proj/.claude");
    sandbox.report_installed(&format!("[{{\"id\":\"{PLUGIN}\",\"scope\":\"user\"}}]"));

    let out = sandbox.install(&[
        "--with-skills",
        "--settings",
        dir.join("s.json").to_str().unwrap(),
    ]);

    assert!(
        out.contains(&format!(
            "already installed in {}; left as is",
            dir.display()
        )),
        "{out}"
    );
}

#[test]
fn without_settings_the_default_config_dir_goes_unnamed() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&["--with-skills"]);

    assert!(
        out.contains("installed the mattpocock-skills plugin\n"),
        "{out}"
    );
}

#[test]
fn a_relative_settings_path_gives_claude_an_absolute_config_dir() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.home.join("proj")).unwrap();
    let mut cmd = sandbox.command(&sandbox.root.join("scripts/install"), &sandbox.path());

    let out = cmd
        .args(["--with-skills", "--settings", "./.claude/settings.json"])
        .current_dir(sandbox.home.join("proj"))
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    let want = format!("config {}", sandbox.home.join("proj/.claude").display());
    let dirs: Vec<String> = sandbox
        .log()
        .into_iter()
        .filter(|l| l.starts_with("config "))
        .collect();
    assert!(
        !dirs.is_empty() && dirs.iter().all(|d| *d == want),
        "{dirs:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "installed the mattpocock-skills plugin in {}",
            sandbox.home.join("proj/.claude").display()
        )),
        "{stdout}"
    );
}
