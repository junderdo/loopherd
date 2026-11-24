//! `loopherd update` against a fake release directory served over `file://`
//! through `LOOPHERD_RELEASE_BASE`, with a `curl` or `wget` on PATH that
//! answers the `releases/latest` redirect.

mod fakes;

use fakes::executable;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GITHUB: &str = "https://github.com/junderdo/loopherd/releases";
const PLUGIN_OLDER: &str = r#"{"checks":[{"name":"herdr plugin","status":"warning","detail":"plugin 0.1.0 differs from loopherd 0.2.0","fix":"herdr plugin update loopherd"}]}"#;
const PLUGIN_CURRENT: &str =
    r#"{"checks":[{"name":"herdr plugin","status":"ok","detail":"0.2.0, enabled","fix":null}]}"#;

/// A temp HOME and config dir, and a release directory laid out like
/// GitHub's `download/<tag>/install.sh`, each installer logging how it ran
/// and installing a `loopherd` whose `doctor --json` prints `doctor.json`.
struct Releases {
    dir: tempfile::TempDir,
}

impl Releases {
    fn new() -> Releases {
        let releases = Releases {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(releases.home()).unwrap();
        fs::create_dir_all(releases.fake_bin()).unwrap();
        executable(
            &releases.fake_bin().join("curl"),
            format!(
                "#!/bin/sh\ncase \"$*\" in\n*redirect_url*) [ -f '{latest}' ] || exit 6; printf '{GITHUB}/tag/%s' \"$(cat '{latest}')\" ;;\n*) exec /usr/bin/curl \"$@\" ;;\nesac\n",
                latest = releases.latest_file().display(),
            ),
        );
        releases
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config")
    }

    fn fake_bin(&self) -> PathBuf {
        self.dir.path().join("fake-bin")
    }

    fn base(&self) -> PathBuf {
        self.dir.path().join("releases")
    }

    fn log_file(&self) -> PathBuf {
        self.dir.path().join("log")
    }

    fn latest_file(&self) -> PathBuf {
        self.dir.path().join("latest")
    }

    fn installed(&self) -> PathBuf {
        self.home().join(".local/bin/loopherd")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log_file()).unwrap_or_default()
    }

    fn latest(&self, tag: &str) {
        fs::write(self.latest_file(), tag).unwrap();
    }

    fn doctor_says(&self, json: &str) {
        fs::write(self.dir.path().join("doctor.json"), json).unwrap();
    }

    fn record(&self, text: &str) {
        fs::create_dir_all(self.config()).unwrap();
        fs::write(self.config().join("install.toml"), text).unwrap();
    }

    fn installed_at(&self, tag: &str) {
        self.record(&format!("kind = \"release\"\ntag = \"{tag}\"\n"));
    }

    fn publish(&self, tag: &str) {
        self.publish_exiting(tag, 0);
    }

    /// Publishes `tag` with an installer that exits `code`.
    fn publish_exiting(&self, tag: &str, code: i32) {
        let dir = self.base().join("download").join(tag);
        fs::create_dir_all(&dir).unwrap();
        let bin = self.installed();
        fs::write(
            dir.join("install.sh"),
            format!(
                "echo \"install.sh {tag} LOOPHERD_VERSION=$LOOPHERD_VERSION LOOPHERD_NO_SPLASH=$LOOPHERD_NO_SPLASH $*\" >> '{log}'\n\
                 echo 'install: installed {bin}'\n\
                 /bin/mkdir -p '{bin_dir}'\n\
                 printf '#!/bin/sh\\n[ \"$1 $2\" != \"doctor --json\" ] || /bin/cat \"%s\"\\n' '{doctor}' > '{bin}.tmp'\n\
                 /bin/chmod 755 '{bin}.tmp'\n\
                 /bin/mv -f '{bin}.tmp' '{bin}'\n\
                 exit {code}\n",
                log = self.log_file().display(),
                bin = bin.display(),
                bin_dir = bin.parent().unwrap().display(),
                doctor = self.dir.path().join("doctor.json").display(),
            ),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_as(
            Path::new(env!("CARGO_BIN_EXE_loopherd")),
            args,
            &self.path(),
        )
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.fake_bin().display())
    }

    fn run_as(&self, binary: &Path, args: &[&str], path: &str) -> Output {
        Command::new(binary)
            .arg("update")
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env(
                "LOOPHERD_RELEASE_BASE",
                format!("file://{}", self.base().display()),
            )
            .output()
            .unwrap()
    }

    /// A PATH with a `wget` that answers like GitHub and copies `file://`
    /// URLs, and `sh`, but no `curl`.
    fn wget_only_path(&self) -> String {
        let bin = self.dir.path().join("wget-bin");
        fs::create_dir_all(&bin).unwrap();
        symlink("/bin/sh", bin.join("sh")).unwrap();
        executable(
            &bin.join("wget"),
            format!(
                "#!/bin/sh\necho \"wget $*\" >> '{log}'\ncase \"$*\" in\n\
                 *--max-redirect=0*) [ -f '{latest}' ] || exit 4; tag=$(/bin/cat '{latest}'); \
                 echo \"  Location: {GITHUB}/tag/$tag\" >&2; echo \"Location: {GITHUB}/tag/$tag [following]\" >&2; exit 8 ;;\n\
                 esac\nout=\nwhile [ $# -gt 0 ]; do case \"$1\" in -O) out=$2; shift ;; file://*) url=${{1#file://}} ;; esac; shift; done\n\
                 /bin/cp \"$url\" \"$out\"\n",
                log = self.log_file().display(),
                latest = self.latest_file().display(),
            ),
        );
        bin.display().to_string()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn behind_it_names_both_tags_then_runs_the_latest_release_s_installer_pinned_to_it() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        releases.log(),
        "install.sh v0.2.0 LOOPHERD_VERSION=v0.2.0 LOOPHERD_NO_SPLASH=1 \n"
    );
    let said = stdout(&out);
    assert!(said.starts_with("v0.1.0 → v0.2.0\n"), "{said}");
    assert!(said.contains("install: installed"), "{said}");
}

#[test]
fn after_installing_it_says_to_restart_any_open_view() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let said = stdout(&releases.run(&[]));

    assert!(
        said.lines()
            .last()
            .unwrap()
            .contains("restart any open `loopherd view`"),
        "{said}"
    );
}

#[test]
fn up_to_date_it_says_so_and_runs_no_installer() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "loopherd is up to date (v0.2.0)\n");
    assert_eq!(releases.log(), "");
    assert!(!releases.installed().exists());
}

#[test]
fn plain_update_never_moves_to_an_older_release() {
    let releases = Releases::new();
    releases.installed_at("v0.10.0");
    releases.latest("v0.9.0");
    releases.publish("v0.9.0");

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "loopherd is up to date (v0.10.0)\n");
    assert_eq!(releases.log(), "");
}

#[test]
fn version_installs_an_older_release_without_asking_for_the_latest() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");
    releases.publish("v0.1.0");

    let out = releases.run(&["--version", "0.1.0"]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("v0.2.0 → v0.1.0\n"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        releases.log(),
        "install.sh v0.1.0 LOOPHERD_VERSION=v0.1.0 LOOPHERD_NO_SPLASH=1 \n"
    );
}

#[test]
fn version_naming_the_current_tag_reinstalls_it() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&["--version", "v0.2.0"]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("v0.2.0 → v0.2.0 (reinstall)\n"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        releases.log(),
        "install.sh v0.2.0 LOOPHERD_VERSION=v0.2.0 LOOPHERD_NO_SPLASH=1 \n"
    );
}

#[test]
fn a_version_that_isnt_a_release_tag_is_refused() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");

    let out = releases.run(&["--version", "latest"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("vX.Y.Z"), "{}", stderr(&out));
    assert_eq!(releases.log(), "");
}

#[test]
fn a_release_that_cant_be_downloaded_fails_naming_its_url() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");

    let out = releases.run(&["--version", "v0.9.9"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("download/v0.9.9/install.sh"),
        "{}",
        stderr(&out)
    );
    assert!(
        !stdout(&out).contains("restart any open"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_failing_installer_fails_the_update_without_the_restart_line() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish_exiting("v0.2.0", 1);

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("v0.2.0"), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains("restart any open"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn with_only_wget_it_reads_the_redirect_and_downloads_with_wget() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");
    let path = releases.wget_only_path();

    let out = releases.run_as(Path::new(env!("CARGO_BIN_EXE_loopherd")), &[], &path);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let log = releases.log();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert!(lines[0].contains("--max-redirect=0"), "{log}");
    assert!(lines[1].contains("download/v0.2.0/install.sh"), "{log}");
    assert_eq!(
        lines[2],
        "install.sh v0.2.0 LOOPHERD_VERSION=v0.2.0 LOOPHERD_NO_SPLASH=1 "
    );
}

#[test]
fn check_behind_exits_1_and_changes_nothing() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&["--check"]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "v0.1.0 → v0.2.0 available; run loopherd update\n"
    );
    assert_eq!(releases.log(), "");
}

#[test]
fn check_up_to_date_exits_0() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");
    releases.latest("v0.2.0");

    let out = releases.run(&["--check"]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "loopherd is up to date (v0.2.0)\n");
}

#[test]
fn check_exits_2_when_it_cant_read_the_latest_release() {
    let releases = Releases::new();
    releases.installed_at("v0.2.0");

    let out = releases.run(&["--check"]);

    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("latest"), "{}", stderr(&out));
}

#[test]
fn check_exits_2_without_a_record() {
    let releases = Releases::new();
    releases.latest("v0.2.0");

    let out = releases.run(&["--check"]);

    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn without_a_record_it_refuses_naming_both_installers() {
    let releases = Releases::new();
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("install.toml"), "{err}");
    assert!(
        err.contains("junderdo.dev/loopherd/install.sh | sh"),
        "{err}"
    );
    assert!(err.contains("scripts/install"), "{err}");
    assert_eq!(releases.log(), "");
}

#[test]
fn a_source_record_is_refused_as_not_built_yet() {
    let releases = Releases::new();
    releases.record("kind = \"source\"\ncheckout = \"/home/me/src/loopherd\"\n");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let out = releases.run(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("source updates aren't built yet"),
        "{}",
        stderr(&out)
    );
    assert_eq!(releases.log(), "");
}

#[test]
fn run_from_elsewhere_it_says_which_loopherd_it_updates() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");

    let said = stdout(&releases.run(&[]));

    assert!(
        said.lines()
            .nth(1)
            .unwrap()
            .contains(&format!("updates {}", releases.installed().display())),
        "{said}"
    );
}

#[test]
fn run_from_the_installed_binary_it_doesnt_say_which() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");
    fs::create_dir_all(releases.installed().parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_loopherd"), releases.installed()).unwrap();

    let out = releases.run_as(&releases.installed(), &[], &releases.path());

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stdout(&out).contains("updates "), "{}", stdout(&out));
}

#[test]
fn an_older_herdr_plugin_is_named_with_its_update_command_not_run() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");
    releases.doctor_says(PLUGIN_OLDER);

    let said = stdout(&releases.run(&[]));

    assert!(said.contains("herdr plugin update loopherd"), "{said}");
}

#[test]
fn a_current_herdr_plugin_isnt_mentioned() {
    let releases = Releases::new();
    releases.installed_at("v0.1.0");
    releases.latest("v0.2.0");
    releases.publish("v0.2.0");
    releases.doctor_says(PLUGIN_CURRENT);

    let said = stdout(&releases.run(&[]));

    assert!(!said.contains("herdr plugin update"), "{said}");
}
