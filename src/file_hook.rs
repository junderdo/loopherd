//! `loopherd file-hook`: a credentials member's PreToolUse hook on Read and
//! Grep. Refuses any path that resolves into `/proc`, where every process's
//! `environ` keeps the GitHub tokens its settings' `env` blanks.

use std::ffi::OsString;
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
    let refuse = |why: String| {
        Some(format!(
            "{tool} of {given} {why}: a credentials member's file tools can't read /proc, \
             where processes' environments keep GitHub tokens"
        ))
    };
    let Some(spellings) = spellings(&absolute) else {
        return refuse("goes through a symlink the hook can't follow".into());
    };
    spellings
        .into_iter()
        // Grep from `/` would search `/proc` too.
        .find(|path| path.starts_with("/proc") || path == Path::new("/"))
        .and_then(|path| refuse(format!("resolves to {}", path.display())))
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

/// Every way `path` can be read: as text, with each symlink's target taken
/// as text, and with every symlink followed. A target counts as written
/// because `/proc/self` in it names the reader, not this hook. `None` when a
/// link can't be read or the links loop.
fn spellings(path: &Path) -> Option<Vec<PathBuf>> {
    const MAX_LINKS: usize = 40;
    let mut found = vec![lexical(path)];
    // The components still to walk, the next one last.
    let mut rest: Vec<OsString> = names(path);
    let mut at = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = rest.pop() {
        if name == ".." {
            at.pop();
            continue;
        }
        let next = at.join(&name);
        if !std::fs::symlink_metadata(&next).is_ok_and(|meta| meta.file_type().is_symlink()) {
            at = next;
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            return None;
        }
        let target = at.join(std::fs::read_link(&next).ok()?);
        let mut whole = target.clone();
        whole.extend(rest.iter().rev());
        found.extend([lexical(&target), lexical(&whole)]);
        rest = names(&whole);
        at = PathBuf::from("/");
    }
    found.push(at);
    Some(found)
}

/// The names in absolute `path`, `..` kept and `.` dropped, the first last.
fn names(path: &Path) -> Vec<OsString> {
    path.components()
        .rev()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
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
    fn a_link_into_proc_that_doesnt_resolve_in_the_hook_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        // In the reading Claude, /proc/self is Claude, whose thread this names.
        std::os::unix::fs::symlink("/proc/self/task/999999999/environ", cwd.path().join("x"))
            .unwrap();
        let hop = cwd.path().join("hop");
        std::os::unix::fs::symlink("x", &hop).unwrap();

        assert!(refusal_in(&read("x", cwd.path())).is_some());
        assert!(refusal_in(&read(hop.to_str().unwrap(), cwd.path())).is_some());
    }

    #[test]
    fn a_link_loop_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("b", cwd.path().join("a")).unwrap();
        std::os::unix::fs::symlink("a", cwd.path().join("b")).unwrap();

        assert!(refusal_in(&read("a", cwd.path())).is_some());
    }

    #[test]
    fn a_link_within_the_worktree_runs() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("docs")).unwrap();
        std::fs::write(cwd.path().join("docs/a.md"), "hi").unwrap();
        std::os::unix::fs::symlink("docs/a.md", cwd.path().join("a")).unwrap();
        std::os::unix::fs::symlink("missing", cwd.path().join("dangling")).unwrap();

        assert_eq!(refusal_in(&read("a", cwd.path())), None);
        assert_eq!(refusal_in(&read("dangling", cwd.path())), None);
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
