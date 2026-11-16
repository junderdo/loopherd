//! `loopherd --version` names the commit a clone build came from.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn version_prints_the_version_and_the_commit_it_was_built_from() {
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let printed = String::from_utf8(out.stdout).unwrap();
    let bare = format!("loopherd {}", env!("CARGO_PKG_VERSION"));
    let Some(commit) = git(&["rev-parse", "--short", "HEAD"]) else {
        assert_eq!(printed.trim(), bare);
        return;
    };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());
    let tagged = git(&["tag", "--points-at", "HEAD"]).is_some_and(|tags| {
        tags.lines()
            .any(|tag| tag == format!("v{}", env!("CARGO_PKG_VERSION")))
    });
    let expected = match (dirty, tagged) {
        (true, _) => format!("{bare} ({commit}-dirty)"),
        (false, true) => bare,
        (false, false) => format!("{bare} ({commit})"),
    };
    assert_eq!(printed.trim(), expected);
}
