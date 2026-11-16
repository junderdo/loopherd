use std::process::Command;

#[path = "src/version.rs"]
mod version;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let git = git_dir().and_then(|dir| {
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

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_dir() -> Option<String> {
    git(&["rev-parse", "--git-dir"])
}

/// Reruns this script when HEAD, the index, a tag, or a tracked source changes.
fn watch(git_dir: &str) {
    let common = git(&["rev-parse", "--git-common-dir"]).unwrap_or_else(|| git_dir.into());
    for path in [
        format!("{git_dir}/HEAD"),
        format!("{git_dir}/index"),
        format!("{common}/refs"),
        format!("{common}/packed-refs"),
        "src".into(),
        "tests".into(),
        "scripts".into(),
        "agents".into(),
        "docs".into(),
        "Cargo.toml".into(),
        "build.rs".into(),
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
}
