//! `loopherd file-hook`: a credentials member's PreToolUse hook on Read and
//! Grep. Refuses any path that resolves into `/proc`, where every process's
//! `environ` keeps the GitHub tokens its settings' `env` blanks.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The reason to refuse hook input `input`, or `None` to let the call run.
/// Input it can't read is refused, so a changed schema can't open `/proc`.
/// `home` expands a leading `~`.
pub fn refusal(input: &[u8], home: Option<&Path>) -> Option<String> {
    let unreadable = || Some("the file hook can't read this call's input".to_string());
    let Ok(input) = serde_json::from_slice::<Value>(input) else {
        return unreadable();
    };
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let given = match tool {
        "Read" => match input["tool_input"]["file_path"].as_str() {
            Some(path) => path,
            None => return unreadable(),
        },
        // Grep searches its cwd without a path.
        "Grep" => input["tool_input"]["path"].as_str().unwrap_or("."),
        _ => return None,
    };
    let expanded = match (given.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(given),
    };
    let cwd = Path::new(input["cwd"].as_str().unwrap_or("/"));
    let absolute = cwd.join(expanded);
    // Read may take `..` as text or follow a symlink first, so both spellings count.
    [lexical(&absolute), resolved(&absolute)]
        .into_iter()
        // Grep from `/` would search `/proc` too.
        .find(|path| path.starts_with("/proc") || path == Path::new("/"))
        .map(|path| {
            format!(
                "{tool} of {given} resolves to {}: a credentials member's file tools can't \
                 read /proc, where processes' environments keep GitHub tokens",
                path.display()
            )
        })
}

/// `path` with `.` and `..` taken out as text, without reading the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` with symlinks followed and `..` taken out, through its longest
/// existing prefix and lexically past it.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let (Some(parent), Some(last)) = (path.parent(), path.components().next_back()) else {
        return path.to_path_buf();
    };
    let mut base = resolved(parent);
    match last {
        Component::ParentDir => {
            base.pop();
        }
        Component::Normal(name) => base.push(name),
        _ => {}
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(path: &str, cwd: &Path) -> Vec<u8> {
        json!({"tool_name": "Read", "tool_input": {"file_path": path}, "cwd": cwd})
            .to_string()
            .into_bytes()
    }

    fn grep(path: Option<&str>, cwd: &Path) -> Vec<u8> {
        let mut input = json!({"pattern": "TOKEN"});
        if let Some(path) = path {
            input["path"] = json!(path);
        }
        json!({"tool_name": "Grep", "tool_input": input, "cwd": cwd})
            .to_string()
            .into_bytes()
    }

    fn refusal_in(input: &[u8]) -> Option<String> {
        refusal(input, Some(Path::new("/home/h")))
    }

    #[test]
    fn reading_any_processs_environ_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        for path in [
            "/proc/self/environ".to_string(),
            format!("/proc/{pid}/environ"),
            format!("/proc/{pid}/task/{pid}/environ"),
            "/proc/thread-self/environ".to_string(),
        ] {
            let reason = refusal_in(&read(&path, cwd.path()));
            assert!(reason.is_some_and(|r| r.contains("/proc")), "{path}");
        }
    }

    #[test]
    fn a_path_that_climbs_or_links_into_proc_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        let link = cwd.path().join("env");
        std::os::unix::fs::symlink("/proc/self/environ", &link).unwrap();
        for path in [
            link.display().to_string(),
            "env".to_string(),
            format!(
                "{}/../../../../../../proc/self/environ",
                cwd.path().display()
            ),
            "/proc/self/root/proc/self/environ".to_string(),
            "/proc/1/../self/environ".to_string(),
            "~/../../proc/self/environ".to_string(),
        ] {
            assert!(refusal_in(&read(&path, cwd.path())).is_some(), "{path}");
        }
    }

    #[test]
    fn dot_dot_after_a_symlinked_dir_counts_as_text_too() {
        let cwd = tempfile::tempdir().unwrap();
        let deep = cwd.path().join("deep/a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::os::unix::fs::symlink(&deep, cwd.path().join("l")).unwrap();
        let climb = "../".repeat(cwd.path().components().count() + 2);

        let reason = refusal_in(&read(&format!("l/{climb}proc/self/environ"), cwd.path()));

        assert!(reason.is_some());
    }

    #[test]
    fn a_tilde_path_is_followed_from_home() {
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/proc/self/environ", home.path().join("env")).unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let reason = refusal(&read("~/env", cwd.path()), Some(home.path()));

        assert!(reason.is_some());
    }

    #[test]
    fn a_missing_path_under_proc_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        assert!(refusal_in(&read("/proc/999999999/environ", cwd.path())).is_some());
    }

    #[test]
    fn grep_in_proc_or_from_the_root_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        for path in ["/proc", "/proc/self", "/"] {
            assert!(
                refusal_in(&grep(Some(path), cwd.path())).is_some(),
                "{path}"
            );
        }
    }

    #[test]
    fn ordinary_reads_and_greps_run() {
        let cwd = tempfile::tempdir().unwrap();
        let file = cwd.path().join("notes.md");
        std::fs::write(&file, "hi").unwrap();
        assert_eq!(refusal_in(&read(file.to_str().unwrap(), cwd.path())), None);
        assert_eq!(refusal_in(&read("notes.md", cwd.path())), None);
        assert_eq!(refusal_in(&read("/etc/hostname", cwd.path())), None);
        assert_eq!(refusal_in(&grep(None, cwd.path())), None);
        assert_eq!(refusal_in(&grep(Some("/etc"), cwd.path())), None);
        assert_eq!(refusal_in(&read("/process-notes", cwd.path())), None);
    }

    #[test]
    fn other_tools_run() {
        let cwd = tempfile::tempdir().unwrap();
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "cat /proc/self/environ"}, "cwd": cwd.path()});
        assert_eq!(refusal_in(bash.to_string().as_bytes()), None);
    }

    #[test]
    fn input_it_cant_read_is_refused() {
        let renamed = json!({"tool_name": "Read", "tool_input": {"path": "/proc/self/environ"}});
        assert!(refusal_in(b"not json").is_some());
        assert!(refusal_in(renamed.to_string().as_bytes()).is_some());
    }
}
