//! `scripts/install.sh`, the one-line installer, run against a fake release
//! directory through `LOOPHERD_RELEASE_BASE`.

mod fakes;

use fakes::executable;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const INSTALL_SH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh");
const DOCTOR_FAILS: &str = "doctor: 8 ok, 0 warnings, 1 failures";

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
        release.publish("latest/download", "v0.2.0");
        release.publish("download/v0.2.0", "v0.2.0");
        release.publish("download/v0.1.0", "v0.1.0");
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
    /// and whose `doctor` reports a failure.
    fn publish(&self, path: &str, tag: &str) {
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
                    "#!/bin/sh\necho \"{tag} $*\" >> '{}'\n[ \"$1\" != doctor ] || echo '{DOCTOR_FAILS}'\n[ ! -f \"{exits}/$1\" ] || exit \"$(cat \"{exits}/$1\")\"\n",
                    self.log.display(),
                    exits = self.exits_dir().display(),
                ),
            );
            let tarball = out.join(format!("loopherd-{target}.tar.gz"));
            run_ok(
                Command::new("tar")
                    .arg("-czf")
                    .arg(&tarball)
                    .arg("-C")
                    .arg(&stage)
                    .arg("loopherd"),
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

    fn run(&self, script: &str, path: &str, envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sh < '{script}'"))
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
    let out = release.run(stamped.to_str().unwrap(), &release.path(), &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        release.log(),
        "v0.1.0 install-hook\nv0.1.0 setup\nv0.1.0 doctor\n"
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
