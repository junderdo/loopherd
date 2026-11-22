//! `scripts/install`'s optional herdr plugin: `--with-herdr-plugin`, the
//! question on a terminal, and when it isn't asked, driven against a stub
//! `herdr` so no test touches the human's herdr.

mod fakes;

use fakes::executable;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const REPO: &str = env!("CARGO_MANIFEST_DIR");
const INSTALL: &str = "herdr plugin install junderdo/loopherd/herdr-plugin";
const UPDATE: &str = "herdr plugin update loopherd";

/// A dir of links to everything on the test's PATH except `herdr`.
fn path_without_herdr(dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    for bin in std::env::var("PATH").unwrap().split(':') {
        let Ok(entries) = fs::read_dir(bin) else {
            continue;
        };
        for entry in entries.flatten() {
            let link = dir.join(entry.file_name());
            if entry.file_name() != "herdr" && !link.exists() {
                std::os::unix::fs::symlink(entry.path(), link).ok();
            }
        }
    }
    dir.display().to_string()
}

/// The installer under a temp root shipping a plugin at 0.3.0, with a temp
/// HOME and state dir, a fake cargo, a `claude` reporting the skills plugin
/// installed, and a `herdr` stub that logs its arguments.
struct Sandbox {
    dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    stub: PathBuf,
    fake_bin: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        for d in ["scripts", "agents", "herdr-plugin"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        executable(
            &root.join("scripts/install"),
            fs::read_to_string(format!("{REPO}/scripts/install")).unwrap(),
        );
        for file in [
            "install-agents.bash",
            "install-skills.bash",
            "install-herdr-plugin.bash",
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
        fs::write(
            root.join("herdr-plugin/herdr-plugin.toml"),
            "id = \"loopherd\"\nname = \"loopherd\"\nversion = \"0.3.0\"\nmin_herdr_version = \"0.7.5\"\n",
        )
        .unwrap();
        let home = dir.path().join("home");
        let stub = dir.path().join("stub");
        let fake_bin = dir.path().join("fake-bin");
        for d in [&home, &stub, &fake_bin] {
            fs::create_dir_all(d).unwrap();
        }
        let built = dir.path().join("built-loopherd");
        executable(&built, "#!/bin/sh\n");
        executable(
            &fake_bin.join("cargo"),
            format!(
                "#!/bin/sh\necho '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"loopherd\"}},\"executable\":\"{}\"}}'\n",
                built.display()
            ),
        );
        executable(
            &fake_bin.join("claude"),
            "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"user\"}]'\n",
        );
        let stub_dir = stub.display();
        executable(
            &fake_bin.join("herdr"),
            format!(
                r#"#!/bin/sh
echo "herdr $*" >> '{stub_dir}/log'
case "$*" in
  "plugin list --plugin loopherd --json")
    [ ! -e '{stub_dir}/list-fails' ] || exit 1
    cat '{stub_dir}/list.json' 2>/dev/null || echo '{{"id":"cli:plugin","result":{{"plugins":[],"type":"plugin_list"}}}}' ;;
  "plugin install"* | "plugin update"*)
    [ ! -e '{stub_dir}/fails' ] || {{ echo "herdr: no network" >&2; exit 1; }}
    echo "herdr says: done" ;;
esac
"#
            ),
        );
        Sandbox {
            dir,
            root,
            home,
            stub,
            fake_bin,
        }
    }

    /// Makes the stub list loopherd's plugin at `version`, from GitHub or,
    /// with `kind` "local", linked.
    fn installed(&self, version: &str, kind: &str) {
        self.listed(&format!(
            r#"{{"plugin_id":"loopherd","enabled":true,"version":"{version}","plugin_root":"/plugins/loopherd","source":{{"kind":"{kind}"}}}}"#
        ));
    }

    /// Makes the stub list `entry` as herdr's one plugin.
    fn listed(&self, entry: &str) {
        fs::write(
            self.stub.join("list.json"),
            format!(
                r#"{{"id":"cli:plugin","result":{{"plugins":[{entry}],"type":"plugin_list"}}}}"#
            ),
        )
        .unwrap();
    }

    fn herdr_calls(&self) -> Vec<String> {
        fs::read_to_string(self.stub.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn changes(&self) -> Vec<String> {
        self.herdr_calls()
            .into_iter()
            .filter(|c| !c.starts_with("herdr plugin list"))
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
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.dir.path().join("config"))
            .env("PATH", path)
            .env("LOOPHERD_NO_SPLASH", "1")
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

    /// Runs the installer on a pty under `path`, typing `answers`, and returns
    /// what it printed; None where util-linux `script` isn't available.
    fn on_terminal_with_path(&self, path: &str, args: &[&str], answers: &str) -> Option<String> {
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
        let mut cmd = self.command(Path::new("script"), path);
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

    /// On a terminal, declines the agent types, then types `answer` to the
    /// plugin question.
    fn on_terminal(&self, answer: &str) -> Option<String> {
        self.on_terminal_with_path(&self.path(), &[], &format!("n\n{answer}\n"))
    }
}

#[test]
fn yes_on_a_terminal_installs_the_plugin_and_prints_herdrs_result() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.on_terminal("y") else {
        return;
    };

    assert!(
        out.contains("Install loopherd's herdr plugin (junderdo/loopherd/herdr-plugin)? [y/N]"),
        "{out}"
    );
    assert_eq!(sandbox.changes(), [INSTALL]);
    assert!(out.contains("herdr says: done"), "{out}");
}

#[test]
fn the_plugin_question_comes_after_the_agent_types_and_skills() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.on_terminal("n") else {
        return;
    };

    let agents = out.find("agent types").unwrap();
    let skills = out
        .find("mattpocock-skills plugin is already installed")
        .unwrap();
    let plugin = out.find("herdr plugin (").unwrap();
    assert!(agents < skills && skills < plugin, "{out}");
}

#[test]
fn no_on_a_terminal_installs_nothing() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.on_terminal("") else {
        return;
    };

    assert!(sandbox.changes().is_empty(), "{:?}", sandbox.changes());
    assert!(out.contains("skipped loopherd's herdr plugin"), "{out}");
}

#[test]
fn an_older_plugin_is_offered_an_update() {
    let sandbox = Sandbox::new();
    sandbox.installed("0.2.9", "github");

    let Some(out) = sandbox.on_terminal("y") else {
        return;
    };

    assert!(
        out.contains(
            "Update loopherd's herdr plugin (installed 0.2.9, this checkout 0.3.0)? [y/N]"
        ),
        "{out}"
    );
    assert_eq!(sandbox.changes(), [UPDATE]);
}

#[test]
fn a_plugin_at_this_checkouts_version_is_left_without_asking() {
    let sandbox = Sandbox::new();
    sandbox.installed("0.3.0", "github");

    let Some(out) = sandbox.on_terminal("y") else {
        return;
    };

    assert!(!out.contains("herdr plugin ("), "{out}");
    assert!(
        out.contains("loopherd's herdr plugin 0.3.0 is already installed; left as is"),
        "{out}"
    );
    assert!(sandbox.changes().is_empty(), "{:?}", sandbox.changes());
}

#[test]
fn a_linked_or_newer_plugin_is_left_without_asking() {
    for (version, kind, says) in [
        (
            "0.2.0",
            "local",
            "linked from /plugins/loopherd; left as is",
        ),
        (
            "0.4.0",
            "github",
            "0.4.0 is newer than this checkout's 0.3.0; left as is",
        ),
    ] {
        let sandbox = Sandbox::new();
        sandbox.installed(version, kind);

        let out = sandbox.install(&["--with-herdr-plugin"]);

        assert!(out.contains(says), "{kind} {version}: {out}");
        assert!(sandbox.changes().is_empty(), "{:?}", sandbox.changes());
    }
}

#[test]
fn without_herdr_on_path_nothing_is_asked() {
    let sandbox = Sandbox::new();
    let bare = path_without_herdr(&sandbox.dir.path().join("bare-bin"));
    fs::remove_file(sandbox.fake_bin.join("herdr")).unwrap();
    let path = format!("{}:{bare}", sandbox.fake_bin.display());

    let Some(out) = sandbox.on_terminal_with_path(&path, &[], "n\ny\n") else {
        return;
    };

    assert!(!out.contains("herdr plugin ("), "{out}");
    assert!(
        out.contains("skipped loopherd's herdr plugin: no herdr on PATH"),
        "{out}"
    );
}

#[test]
fn with_herdr_plugin_installs_asking_nothing_else() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&["--with-herdr-plugin"]);

    assert_eq!(sandbox.changes(), [format!("{INSTALL} --yes")]);
    assert!(!out.contains("agent types"), "{out}");
    assert!(!out.contains("mattpocock-skills"), "{out}");
}

#[test]
fn with_herdr_plugin_updates_an_older_one() {
    let sandbox = Sandbox::new();
    sandbox.installed("0.2.9", "github");

    sandbox.install(&["--with-herdr-plugin"]);

    assert_eq!(sandbox.changes(), [format!("{UPDATE} --yes")]);
}

#[test]
fn a_failed_plugin_install_is_reported_and_the_install_goes_on() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("fails"), "").unwrap();

    let out = sandbox.run(&["--with-herdr-plugin"]);

    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("herdr: no network"), "{stderr}");
    assert!(
        stderr.contains("install: herdr plugin install failed"),
        "{stderr}"
    );
    assert!(sandbox.home.join(".local/bin/loopherd").exists());
}

#[test]
fn a_plugin_list_herdr_cant_answer_skips_the_plugin() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("list-fails"), "").unwrap();

    let out = sandbox.install(&["--with-herdr-plugin"]);

    assert!(
        out.contains("skipped loopherd's herdr plugin: herdr couldn't list its plugins"),
        "{out}"
    );
    assert!(sandbox.changes().is_empty(), "{:?}", sandbox.changes());
}

#[test]
fn without_a_terminal_or_the_flag_nothing_is_asked_or_installed() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert!(
        sandbox.herdr_calls().is_empty(),
        "{:?}",
        sandbox.herdr_calls()
    );
    assert!(
        out.contains(
            "skipped loopherd's herdr plugin (no terminal); add --with-herdr-plugin to install it"
        ),
        "{out}"
    );
}

#[test]
fn no_extras_skips_the_plugin_question() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.on_terminal_with_path(&sandbox.path(), &["--no-extras"], "y\ny\n")
    else {
        return;
    };

    assert!(!out.contains("herdr plugin"), "{out}");
    assert!(
        sandbox.herdr_calls().is_empty(),
        "{:?}",
        sandbox.herdr_calls()
    );
}

#[test]
fn no_extras_refuses_with_herdr_plugin() {
    let sandbox = Sandbox::new();

    let out = sandbox.run(&["--no-extras", "--with-herdr-plugin"]);

    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--with-herdr-plugin"),
        "{out:?}"
    );
    assert!(sandbox.herdr_calls().is_empty());
}

#[test]
fn a_settings_run_never_installs_into_the_humans_herdr() {
    let sandbox = Sandbox::new();
    let settings = sandbox.dir.path().join("claude/settings.json");

    let out = sandbox.install(&[
        "--with-herdr-plugin",
        "--settings",
        settings.to_str().unwrap(),
    ]);

    assert!(
        sandbox.herdr_calls().is_empty(),
        "{:?}",
        sandbox.herdr_calls()
    );
    assert!(
        out.contains("skipped loopherd's herdr plugin: a --settings run never installs it"),
        "{out}"
    );
}

#[test]
fn help_names_the_question_and_the_flag() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&["--help"]);

    assert!(out.contains("--with-herdr-plugin"), "{out}");
    assert!(out.contains("loopherd's herdr plugin"), "{out}");
}

#[test]
fn a_checkout_without_the_plugin_manifest_skips_the_plugin() {
    let sandbox = Sandbox::new();
    fs::remove_file(sandbox.root.join("herdr-plugin/herdr-plugin.toml")).unwrap();

    let out = sandbox.install(&["--with-herdr-plugin"]);

    assert!(
        out.contains("skipped loopherd's herdr plugin: no version in"),
        "{out}"
    );
    assert!(
        sandbox.herdr_calls().is_empty(),
        "{:?}",
        sandbox.herdr_calls()
    );
}

#[test]
fn a_linked_plugin_without_a_version_is_still_left_as_is() {
    let sandbox = Sandbox::new();
    sandbox.listed(
        r#"{"plugin_id":"loopherd","enabled":true,"plugin_root":"/plugins/loopherd","source":{"kind":"local"}}"#,
    );

    let out = sandbox.install(&["--with-herdr-plugin"]);

    assert!(
        out.contains("linked from /plugins/loopherd; left as is"),
        "{out}"
    );
    assert!(sandbox.changes().is_empty(), "{:?}", sandbox.changes());
}
