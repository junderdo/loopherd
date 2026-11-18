use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `contents` to an executable `path` from a child process. A file this
/// process held open for writing can leak into a sibling test's fork and make
/// running `path` fail with ETXTBSY.
pub(crate) fn executable(path: &Path, contents: impl AsRef<[u8]>) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(contents.as_ref()).unwrap();
    drop(stdin);
    assert!(
        child.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_a_file_that_runs() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("a fake");

        executable(&fake, "#!/bin/sh\necho ran \"$@\"\n");

        let mode = std::fs::metadata(&fake).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let out = Command::new(&fake).arg("x").output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "ran x\n");
    }
}
