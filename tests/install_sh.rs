//! `scripts/install.sh`, the one-line installer, run against a fake release
//! directory through `LOOPHERD_RELEASE_BASE`.

mod fakes;

use fakes::executable;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const INSTALL_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh");
const DOCTOR_FAILS: &str = "doctor: 8 ok, 0 warnings, 1 failures";
const REPO: &str = env!("CARGO_MANIFEST_DIR");
const GITHUB: &str = "https://github.com/junderdo/loopherd/releases";
const AGENTS: [&str; 4] = ["builder", "researcher", "reviewer", "scout"];

/// The files beside `loopherd` in a release tarball that carries the extras.
fn stage_extras(stage: &Path) {
    fs::create_dir_all(stage.join("scripts")).unwrap();
    fs::create_dir_all(stage.join("agents")).unwrap();
    fs::create_dir_all(stage.join("herdr-plugin")).unwrap();
    for script in [
        "install-extras",
        "install-splash.bash",
        "install-agents.bash",
        "install-skills.bash",
        "install-herdr-plugin.bash",
        "install-herdr-keys.bash",
    ] {
        executable(
            &stage.join("scripts").join(script),
            fs::read(format!("{REPO}/scripts/{script}")).unwrap(),
        );
    }
    for name in AGENTS {
        fs::copy(
            format!("{REPO}/agents/{name}.md"),
            stage.join(format!("agents/{name}.md")),
        )
        .unwrap();
    }
    fs::copy(
        format!("{REPO}/herdr-plugin/herdr-plugin.toml"),
        stage.join("herdr-plugin/herdr-plugin.toml"),
    )
    .unwrap();
}

/// A temp HOME, a fake `uname`, and a release directory laid out like
/// GitHub's: `latest/download/<file>` and `download/<tag>/<file>`.
struct Release {
    dir: tempfile::TempDir,
    home: PathBuf,
    fake_bin: PathBuf,
    log: PathBuf,
}

impl Release {
    fn new(os: &str, arch: &str) -> Release {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        executable(
            &fake_bin.join("uname"),
            format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo {os} ;; -m) echo {arch} ;; *) exit 1 ;; esac\n"
            ),
        );
        let log = dir.path().join("log");
        let release = Release {
            dir,
            home,
            fake_bin,
            log,
        };
        fs::create_dir_all(release.exits_dir()).unwrap();
        release.exits("doctor", 1);
        release.publish("latest/download", "v0.2.0", true);
        release.publish("download/v0.2.0", "v0.2.0", true);
        release.publish("download/v0.1.0", "v0.1.0", false);
        release
    }

    fn exits_dir(&self) -> PathBuf {
        self.dir.path().join("exits")
    }

    /// Sets the exit code of the fake `loopherd`'s `subcommand`.
    fn exits(&self, subcommand: &str, code: i32) {
        fs::write(self.exits_dir().join(subcommand), code.to_string()).unwrap();
    }

    fn base(&self) -> PathBuf {
        self.dir.path().join("releases")
    }

    /// One tarball per target, each holding a `loopherd` that logs `<tag> <args>`
    /// and whose `doctor` reports a failure, and with `extras` the files beside it.
    fn publish(&self, path: &str, tag: &str, extras: bool) {
        let out = self.base().join(path);
        fs::create_dir_all(&out).unwrap();
        for target in [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            let stage = self.dir.path().join("stage").join(tag).join(target);
            fs::create_dir_all(&stage).unwrap();
            executable(
                &stage.join("loopherd"),
                format!(
                    "#!/bin/sh\n[ \"$1\" != --version ] || {{ echo 'loopherd {version}'; exit; }}\necho \"{tag} $*\" >> '{}'\n[ \"$1\" != doctor ] || echo '{DOCTOR_FAILS}'\n[ ! -f \"{exits}/$1\" ] || exit \"$(cat \"{exits}/$1\")\"\n",
                    self.log.display(),
                    exits = self.exits_dir().display(),
                    version = tag.trim_start_matches('v'),
                ),
            );
            if extras {
                stage_extras(&stage);
            }
            let tarball = out.join(format!("loopherd-{target}.tar.gz"));
            run_ok(
                Command::new("tar")
                    .arg("-czf")
                    .arg(&tarball)
                    .arg("-C")
                    .arg(&stage)
                    .arg("."),
            );
        }
        let sums = run_ok(
            Command::new("sh")
                .arg("-c")
                .arg("sha256sum loopherd-*.tar.gz")
                .current_dir(&out),
        );
        fs::write(out.join("SHA256SUMS"), sums).unwrap();
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/loopherd")
    }

    fn record(&self) -> PathBuf {
        self.home.join(".config/loopherd/install.toml")
    }

    /// A `curl` that fetches GitHub's release URLs from the fake release, for
    /// a run with `LOOPHERD_RELEASE_BASE` empty.
    fn serve_github(&self) {
        executable(
            &self.fake_bin.join("curl"),
            format!(
                "#!/bin/sh\nfor a; do shift; set -- \"$@\" \"$(echo \"$a\" | sed 's|^{GITHUB}|file://{}|')\"; done\nexec /usr/bin/curl \"$@\"\n",
                self.base().display()
            ),
        );
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// PATH is the system dirs, so herdr, claude and ~/.local/bin are all missing.
    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.fake_bin.display())
    }

    fn install(&self, envs: &[(&str, &str)]) -> Output {
        self.run(INSTALL_SH, &self.path(), envs)
    }

    fn install_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.run_args(INSTALL_SH, &self.path(), args, envs)
    }

    fn run(&self, script: &str, path: &str, envs: &[(&str, &str)]) -> Output {
        self.run_args(script, path, &[], envs)
    }

    /// Like `curl | sh -s -- <args>`, in a new session so there's no /dev/tty
    /// for the installer to ask on.
    fn run_args(&self, script: &str, path: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sh -s -- \"$@\" < '{script}'"))
            .arg("sh")
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env(
                "LOOPHERD_RELEASE_BASE",
                format!("file://{}", self.base().display()),
            )
            .stdin(Stdio::null());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.output().unwrap()
    }
}

fn run_ok(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn installs_the_latest_release_then_its_hooks_and_setup() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[]);
    assert!(out.status.success(), "{}", text(&out));
    let mode = fs::metadata(release.installed())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        release.log(),
        "v0.2.0 install-hook\nv0.2.0 setup\nv0.2.0 doctor\n"
    );
    assert!(text(&out).contains("loopherd herds"), "{}", text(&out));
    assert!(
        !release.record().exists(),
        "a fake release base writes no install record"
    );
}

#[test]
fn an_install_from_github_records_the_installed_tag_in_the_config_dir() {
    let release = Release::new("Linux", "x86_64");
    release.serve_github();
    let out = release.install(&[("LOOPHERD_RELEASE_BASE", "")]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(release.record()).unwrap(),
        "kind = \"release\"\ntag = \"v0.2.0\"\n"
    );
}

#[test]
fn a_pinned_install_from_github_records_the_pinned_tag_under_loopherd_config_dir() {
    let release = Release::new("Linux", "x86_64");
    release.serve_github();
    let config = release.dir.path().join("config");
    let out = release.install(&[
        ("LOOPHERD_RELEASE_BASE", ""),
        ("LOOPHERD_VERSION", "0.1.0"),
        ("LOOPHERD_CONFIG_DIR", config.to_str().unwrap()),
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(config.join("install.toml")).unwrap(),
        "kind = \"release\"\ntag = \"v0.1.0\"\n"
    );
    assert!(!release.record().exists());
}

#[test]
fn a_failed_install_from_github_records_nothing() {
    let release = Release::new("Linux", "x86_64");
    release.serve_github();
    fs::remove_file(release.base().join("latest/download/SHA256SUMS")).unwrap();
    let out = release.install(&[("LOOPHERD_RELEASE_BASE", "")]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(!release.record().exists());
}

#[test]
fn prints_a_failing_doctor_last_and_still_succeeds() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim_end().ends_with(DOCTOR_FAILS), "{stdout}");
}

#[test]
fn succeeds_when_doctor_cannot_run() {
    let release = Release::new("Linux", "x86_64");
    release.exits("doctor", 2);
    let out = release.install(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[test]
fn a_failed_install_hook_fails_the_install_before_doctor() {
    let release = Release::new("Linux", "x86_64");
    release.exits("install-hook", 1);
    let out = release.install(&[]);
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(release.log(), "v0.2.0 install-hook\n");
}

#[test]
fn picks_the_tarball_for_a_mac() {
    let release = Release::new("Darwin", "arm64");
    let out = release.install(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("aarch64-apple-darwin"),
        "{}",
        text(&out)
    );
}

#[test]
fn rerunning_installs_again() {
    let release = Release::new("Linux", "aarch64");
    assert!(release.install(&[]).status.success());
    let out = release.install(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(release.log().lines().count(), 6);
}

#[test]
fn a_checksum_mismatch_refuses_and_leaves_the_old_binary() {
    let release = Release::new("Linux", "x86_64");
    fs::create_dir_all(release.installed().parent().unwrap()).unwrap();
    executable(&release.installed(), "#!/bin/sh\necho old\n");
    let sums = release.base().join("latest/download/SHA256SUMS");
    let tampered: String = fs::read_to_string(&sums)
        .unwrap()
        .lines()
        .map(|line| match line.split_once("  ") {
            Some((_, file)) if file == "loopherd-x86_64-unknown-linux-musl.tar.gz" => {
                format!("{}  {file}\n", "0".repeat(64))
            }
            _ => format!("{line}\n"),
        })
        .collect();
    fs::write(&sums, tampered).unwrap();
    let out = release.install(&[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("checksum"), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(release.installed()).unwrap(),
        "#!/bin/sh\necho old\n"
    );
    assert_eq!(release.log(), "");
}

#[test]
fn an_unsupported_arch_names_the_supported_targets() {
    let release = Release::new("Linux", "riscv64");
    let out = release.install(&[]);
    assert!(!out.status.success());
    let said = text(&out);
    assert!(said.contains("riscv64"), "{said}");
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        assert!(said.contains(target), "{said}");
    }
    assert!(!release.installed().exists());
}

#[test]
fn loopherd_version_pins_a_release() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[("LOOPHERD_VERSION", "v0.1.0")]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        release.log(),
        "v0.1.0 install-hook\nv0.1.0 setup\nv0.1.0 doctor\n"
    );
    assert!(!release.record().exists());
}

#[test]
fn a_script_stamped_by_a_release_installs_that_release() {
    let release = Release::new("Linux", "x86_64");
    let stamped = release.dir.path().join("install.sh");
    let script = fs::read_to_string(INSTALL_SH).unwrap();
    assert!(script.contains("\nrelease_tag=\n"));
    fs::write(
        &stamped,
        script.replace("\nrelease_tag=\n", "\nrelease_tag=v0.1.0\n"),
    )
    .unwrap();
    release.serve_github();
    let out = release.run(
        stamped.to_str().unwrap(),
        &release.path(),
        &[("LOOPHERD_RELEASE_BASE", "")],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        release.log(),
        "v0.1.0 install-hook\nv0.1.0 setup\nv0.1.0 doctor\n"
    );
    assert_eq!(
        fs::read_to_string(release.record()).unwrap(),
        "kind = \"release\"\ntag = \"v0.1.0\"\n"
    );
}

#[test]
fn warns_about_what_isnt_on_path() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("herdr"), "{said}");
    assert!(said.contains("claude"), "{said}");
    assert!(said.contains(".local/bin"), "{said}");
}

#[test]
fn a_missing_checksum_tool_is_named() {
    let release = Release::new("Linux", "x86_64");
    let bin = release.dir.path().join("bare-bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(release.fake_bin.join("uname"), bin.join("uname")).unwrap();
    for tool in [
        "sh", "curl", "tar", "gzip", "mktemp", "mkdir", "rm", "mv", "cp", "chmod", "awk",
    ] {
        let found = run_ok(
            Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool}")),
        );
        let found = String::from_utf8(found).unwrap();
        symlink(found.trim(), bin.join(tool)).unwrap();
    }
    let out = release.run(INSTALL_SH, bin.to_str().unwrap(), &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("sha256sum or shasum"), "{}", text(&out));
    assert!(!release.installed().exists());
}

#[test]
fn without_a_terminal_each_offer_is_skipped_saying_how_to_be_asked() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    for line in [
        "skipped loopherd's agent types (no terminal); run this installer again in a terminal to be asked",
        "skipped the mattpocock-skills plugin (no terminal); run this installer again in a terminal to be asked",
        "skipped loopherd's herdr plugin (no terminal); run this installer again in a terminal to be asked",
        "skipped herdr key bindings (no terminal); run this installer again in a terminal to be asked",
    ] {
        assert!(said.contains(line), "{line}\n---\n{said}");
    }
    assert!(!said.contains("--with-"), "{said}");
    assert!(!release.home.join(".claude/agents").exists());
    assert_eq!(
        release.log(),
        "v0.2.0 install-hook\nv0.2.0 setup\nv0.2.0 doctor\n"
    );
}

impl Release {
    /// The installer as `curl | sh` on a pty fed `typed`, or None where
    /// util-linux `script` isn't available.
    fn install_on_terminal(&self, envs: &[(&str, &str)], typed: &str) -> Option<String> {
        self.install_on_terminal_with_path(&self.path(), envs, typed)
    }

    fn install_on_terminal_with_path(
        &self,
        path: &str,
        envs: &[(&str, &str)],
        typed: &str,
    ) -> Option<String> {
        let util_linux = Command::new("script")
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("util-linux"));
        if !util_linux {
            return None;
        }
        let shell = format!("stty cols 100 rows 30; sh -s < '{INSTALL_SH}'");
        let mut cmd = Command::new("script");
        cmd.args(["-qec", &shell, "/dev/null"])
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("TERM", "xterm-256color")
            .env(
                "LOOPHERD_RELEASE_BASE",
                format!("file://{}", self.base().display()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, typed.as_bytes()).unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", text(&out));
        Some(text(&out))
    }

    fn agents_dir(&self) -> PathBuf {
        self.home.join(".claude/agents")
    }
}

const ALT_SCREEN: &str = "\x1b[?1049h";
const NO_SPLASH: (&str, &str) = ("LOOPHERD_NO_SPLASH", "1");

#[test]
fn on_a_terminal_the_offers_are_asked_there_and_yes_installs() {
    let release = Release::new("Linux", "x86_64");
    let Some(said) = release.install_on_terminal(&[NO_SPLASH], "y\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };
    assert!(said.contains("Install loopherd's agent types"), "{said}");
    for name in AGENTS {
        assert!(
            release.agents_dir().join(format!("{name}.md")).exists(),
            "{said}"
        );
    }
}

#[test]
fn on_a_terminal_the_splash_plays_unless_turned_off() {
    let release = Release::new("Linux", "x86_64");
    let Some(said) = release.install_on_terminal(&[], "\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };
    assert!(said.contains(ALT_SCREEN), "{said}");
    let said = release.install_on_terminal(&[NO_SPLASH], "\n").unwrap();
    assert!(!said.contains(ALT_SCREEN), "{said}");
}

#[test]
fn any_argument_is_refused_before_installing_anything() {
    for arg in [
        "--no-splash",
        "--with-agents",
        "--with-skills",
        "--with-herdr-plugin",
        "--with-herdr-keys",
        "--no-extras",
        "anything",
    ] {
        let release = Release::new("Linux", "x86_64");
        let out = release.install_with(&[arg], &[]);
        assert!(!out.status.success(), "{arg}");
        assert_eq!(
            text(&out),
            "install: takes no options; it asks about each extra as it installs\n",
            "{arg}"
        );
        assert!(!release.installed().exists(), "{arg}");
    }
}

#[test]
fn a_release_without_extras_still_installs_and_says_so() {
    let release = Release::new("Linux", "x86_64");
    let out = release.install(&[("LOOPHERD_VERSION", "v0.1.0")]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("this release ships no extras"), "{said}");
}

#[test]
fn without_a_terminal_installed_extras_are_shown_as_installed() {
    let release = Release::new("Linux", "x86_64");
    release.agents_and_skills_installed();
    let version = shipped_plugin_version();
    release.herdr_lists_plugin(&version, "github");

    let out = release.install(&[]);
    let said = text(&out);
    assert!(out.status.success(), "{said}");
    for line in [
        format!(
            "loopherd's agent types are already installed in {}; left as is",
            release.agents_dir().display()
        ),
        "the mattpocock-skills plugin is already installed; left as is".to_string(),
        format!("loopherd's herdr plugin {version} is already installed; left as is"),
    ] {
        assert!(said.contains(&line), "{line}\n---\n{said}");
    }
}

/// The herdr plugin version a release ships, which is loopherd's own.
fn shipped_plugin_version() -> String {
    let manifest = fs::read_to_string(format!("{REPO}/herdr-plugin/herdr-plugin.toml")).unwrap();
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .unwrap()
        .trim_end_matches('"')
        .to_string()
}

impl Release {
    /// The fake bin, then links to everything in the system dirs but jq.
    fn path_without_jq(&self) -> String {
        let bin = self.dir.path().join("no-jq-bin");
        fs::create_dir_all(&bin).unwrap();
        for dir in ["/usr/bin", "/bin"] {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let link = bin.join(entry.file_name());
                if entry.file_name() != "jq" && fs::symlink_metadata(&link).is_err() {
                    symlink(entry.path(), link).unwrap();
                }
            }
        }
        format!("{}:{}", self.fake_bin.display(), bin.display())
    }
}

const NEEDS_JQ: &str = "skipped the mattpocock-skills plugin and the herdr offers: they need jq; install it, then run this installer again";

#[test]
fn without_jq_the_agent_types_are_still_offered_and_the_rest_skipped_naming_it() {
    let release = Release::new("Linux", "x86_64");

    let out = release.run(INSTALL_SH, &release.path_without_jq(), &[]);

    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("skipped loopherd's agent types (no terminal); run this installer again in a terminal to be asked"),
        "{said}"
    );
    assert!(said.contains(NEEDS_JQ), "{said}");
    assert!(release.installed().exists());
}

#[test]
fn without_jq_on_a_terminal_yes_installs_the_agent_types() {
    let release = Release::new("Linux", "x86_64");
    let Some(said) =
        release.install_on_terminal_with_path(&release.path_without_jq(), &[NO_SPLASH], "y\n")
    else {
        eprintln!("skipped: needs util-linux script");
        return;
    };
    assert!(said.contains(NEEDS_JQ), "{said}");
    for name in AGENTS {
        assert!(
            release.agents_dir().join(format!("{name}.md")).exists(),
            "{said}"
        );
    }
}

impl Release {
    /// The agent types and the skills plugin already installed, so neither is asked about.
    fn agents_and_skills_installed(&self) {
        fs::create_dir_all(self.agents_dir()).unwrap();
        for name in AGENTS {
            fs::copy(
                format!("{REPO}/agents/{name}.md"),
                self.agents_dir().join(format!("{name}.md")),
            )
            .unwrap();
        }
        executable(
            &self.fake_bin.join("claude"),
            "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"user\"}]'\n",
        );
    }

    /// A `herdr` on PATH that logs its arguments and lists loopherd's plugin
    /// at `version` from `kind` ("github" or "local").
    fn herdr_lists_plugin(&self, version: &str, kind: &str) {
        executable(
            &self.fake_bin.join("herdr"),
            format!(
                "#!/bin/sh\necho \"herdr $*\" >> '{log}'\n[ \"$1\" != --version ] || {{ echo 'herdr 0.9.5'; exit 0; }}\n[ \"$1 $2\" = 'plugin list' ] || exit 0\necho '{{\"result\":{{\"plugins\":[{{\"plugin_id\":\"loopherd\",\"version\":\"{version}\",\"source\":{{\"kind\":\"{kind}\"}},\"plugin_root\":\"/plugins/loopherd\"}}]}}}}'\n",
                log = self.herdr_log().display(),
            ),
        );
    }

    /// A `herdr` on PATH that logs its arguments and lists no loopherd plugin;
    /// its `plugin install --help` names `--ref` when `takes_ref`.
    fn herdr_without_plugin(&self, takes_ref: bool) {
        let help = if takes_ref {
            "      --ref <REF>"
        } else {
            "  -y, --yes"
        };
        executable(
            &self.fake_bin.join("herdr"),
            format!(
                "#!/bin/sh\necho \"herdr $*\" >> '{log}'\ncase \"$*\" in\n  --version) echo 'herdr 0.9.5' ;;\n  'plugin install --help') echo '{help}' ;;\n  'plugin list'*) echo '{{\"result\":{{\"plugins\":[]}}}}' ;;\nesac\n",
                log = self.herdr_log().display(),
            ),
        );
    }

    fn herdr_log(&self) -> PathBuf {
        self.dir.path().join("herdr-log")
    }

    fn herdr_changes(&self) -> Vec<String> {
        fs::read_to_string(self.herdr_log())
            .unwrap_or_default()
            .lines()
            .filter(|call| {
                !call.starts_with("herdr plugin list")
                    && *call != "herdr --version"
                    && *call != "herdr plugin install --help"
            })
            .map(String::from)
            .collect()
    }
}

#[test]
fn without_a_terminal_an_older_herdr_plugin_is_skipped_saying_how_to_be_asked() {
    let release = Release::new("Linux", "x86_64");
    release.herdr_lists_plugin("0.1.1", "github");

    let out = release.install(&[]);

    let said = text(&out);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains(
            "skipped loopherd's herdr plugin 0.1.1 (no terminal); run this installer again in a terminal to be asked"
        ),
        "{said}"
    );
    assert_eq!(release.herdr_changes(), Vec::<String>::new());
}

#[test]
fn on_a_terminal_yes_updates_an_older_herdr_plugin() {
    let release = Release::new("Linux", "x86_64");
    release.agents_and_skills_installed();
    release.herdr_lists_plugin("0.1.1", "github");

    let Some(said) = release.install_on_terminal(&[NO_SPLASH], "y\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let ours = shipped_plugin_version();
    assert!(
        said.contains(&format!(
            "Update loopherd's herdr plugin (installed 0.1.1, this release {ours})? [y/N]"
        )),
        "{said}"
    );
    assert_eq!(
        release.herdr_changes(),
        vec!["herdr plugin update loopherd".to_string()],
        "{said}"
    );
}

#[test]
fn on_a_terminal_an_older_herdr_plugin_is_left_by_default() {
    let release = Release::new("Linux", "x86_64");
    release.agents_and_skills_installed();
    release.herdr_lists_plugin("0.1.1", "github");

    let Some(said) = release.install_on_terminal(&[NO_SPLASH], "\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    assert!(said.contains("skipped loopherd's herdr plugin"), "{said}");
    assert_eq!(release.herdr_changes(), Vec::<String>::new(), "{said}");
}

#[test]
fn on_a_terminal_a_current_newer_or_linked_herdr_plugin_is_left_as_is() {
    let ours = shipped_plugin_version();
    for (version, kind, line) in [
        (
            ours.as_str(),
            "github",
            format!("loopherd's herdr plugin {ours} is already installed; left as is"),
        ),
        (
            "999.0.0",
            "github",
            format!(
                "loopherd's herdr plugin 999.0.0 is newer than this release's {ours}; left as is"
            ),
        ),
        (
            "0.1.1",
            "local",
            "loopherd's herdr plugin is linked from /plugins/loopherd; left as is".to_string(),
        ),
    ] {
        let release = Release::new("Linux", "x86_64");
        release.agents_and_skills_installed();
        release.herdr_lists_plugin(version, kind);

        let Some(said) = release.install_on_terminal(&[NO_SPLASH], "y\n") else {
            eprintln!("skipped: needs util-linux script");
            return;
        };

        assert!(said.contains(&line), "{line}\n---\n{said}");
        assert!(!said.contains("Update loopherd's herdr plugin"), "{said}");
        assert_eq!(release.herdr_changes(), Vec::<String>::new(), "{said}");
    }
}

const UPDATING: (&str, &str) = ("LOOPHERD_UPDATE", "1");

#[test]
fn on_a_terminal_an_update_offers_nothing_and_plays_no_splash() {
    let release = Release::new("Linux", "x86_64");
    release.herdr_lists_plugin("0.1.1", "github");

    let Some(said) = release.install_on_terminal(&[UPDATING], "y\ny\ny\ny\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    assert!(!said.contains(ALT_SCREEN), "{said}");
    assert!(!said.contains("[y/N]"), "{said}");
    assert!(!said.contains("skipped"), "{said}");
    assert!(!release.agents_dir().exists(), "{said}");
    assert_eq!(release.herdr_changes(), Vec::<String>::new(), "{said}");
    assert_eq!(
        release.log(),
        "v0.2.0 install-hook\nv0.2.0 setup\nv0.2.0 doctor\n"
    );
}

#[test]
fn an_update_refreshes_the_agent_types_loopherd_installed_and_names_the_skills_update() {
    let release = Release::new("Linux", "x86_64");
    release.agents_and_skills_installed();
    let builder = release.agents_dir().join("builder.md");
    fs::write(&builder, "an older builder\n").unwrap();
    let sha = run_ok(Command::new("sha256sum").arg(&builder));
    let sha = String::from_utf8(sha).unwrap();
    let ledger = release.home.join(".local/state/loopherd/agents.sha256");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::write(
        &ledger,
        format!("{} {}\n", sha.split(' ').next().unwrap(), builder.display()),
    )
    .unwrap();

    let Some(said) = release.install_on_terminal(&[UPDATING], "y\ny\ny\ny\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    assert_eq!(
        fs::read_to_string(&builder).unwrap(),
        fs::read_to_string(format!("{REPO}/agents/builder.md")).unwrap(),
        "{said}"
    );
    assert!(
        said.contains("claude plugin update mattpocock-skills@claude-plugins-official"),
        "{said}"
    );
    assert!(!said.contains("[y/N]"), "{said}");
}

#[test]
fn on_a_terminal_yes_installs_the_herdr_plugin_at_the_installed_release() {
    let release = Release::new("Linux", "x86_64");
    release.herdr_without_plugin(true);

    let Some(said) = release.install_on_terminal(&[NO_SPLASH], "n\ny\nn\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    assert_eq!(
        release.herdr_changes(),
        vec!["herdr plugin install junderdo/loopherd/herdr-plugin --ref v0.2.0".to_string()],
        "{said}"
    );
}

#[test]
fn a_herdr_that_cant_install_at_a_ref_installs_the_default_branch_and_says_so() {
    let release = Release::new("Linux", "x86_64");
    release.herdr_without_plugin(false);

    let Some(said) = release.install_on_terminal(&[NO_SPLASH], "n\ny\nn\n") else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    assert!(
        said.contains("herdr 0.9.5 can't install a plugin at a ref, so loopherd's herdr plugin comes from GitHub's default branch, not v0.2.0"),
        "{said}"
    );
    assert_eq!(
        release.herdr_changes(),
        vec!["herdr plugin install junderdo/loopherd/herdr-plugin".to_string()],
        "{said}"
    );
}
