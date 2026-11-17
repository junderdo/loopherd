use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `script` to an executable `path` from a child process. A file this
/// process held open for writing can leak into a sibling test's fork and make
/// running `path` fail with ETXTBSY.
pub fn executable(path: &Path, script: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(script.as_bytes()).unwrap();
    drop(stdin);
    assert!(
        child.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
}
