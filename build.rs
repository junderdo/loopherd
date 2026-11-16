use std::path::Path;
use std::process::Command;

#[path = "src/version.rs"]
mod version;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let git = git(&["rev-parse", "--git-dir"]).and_then(|dir| {
        watch(&dir);
        Some(version::Git {
            commit: git(&["rev-parse", "--short", "HEAD"])?,
            dirty: !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
            tagged: git(&["tag", "--points-at", "HEAD"])?
                .lines()
                .any(|tag| tag == format!("v{version}")),
        })
    });
    println!(
        "cargo:rustc-env=LOOPHERD_VERSION={}",
        version::describe(version, git)
    );
}

// --no-optional-locks: status otherwise refreshes the index, taking its lock
// and making the next build rerun this script.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Reruns this script when a tracked file, HEAD, this branch, or a tag changes.
fn watch(git_dir: &str) {
    let common = git(&["rev-parse", "--git-common-dir"]).unwrap_or_else(|| git_dir.into());
    let mut paths = vec![
        format!("{git_dir}/HEAD"),
        format!("{git_dir}/index"),
        format!("{common}/refs/tags"),
    ];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(format!("{common}/{branch}"));
    }
    // Cargo counts a missing watched file as changed, so these go in only when present.
    paths.push(format!("{common}/packed-refs"));
    paths.retain(|path| Path::new(path).exists());
    paths.extend(
        git(&["ls-files"])
            .unwrap_or_default()
            .lines()
            .map(String::from),
    );
    for path in paths {
        println!("cargo:rerun-if-changed={path}");
    }
}
