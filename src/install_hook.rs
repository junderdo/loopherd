//! `loopherd install-hook`: merge loopherd's Stop, StopFailure, SessionStart,
//! PreToolUse and UserPromptSubmit hooks into Claude's user settings, or take
//! them out with `--remove`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

struct Hook {
    event: &'static str,
    subcommand: &'static str,
    matcher: Option<&'static str>,
    timeout: u64,
    /// `watch` refuses to run without it.
    watched: bool,
}

const HOOKS: [Hook; 5] = [
    Hook {
        event: "Stop",
        subcommand: "stop-hook",
        matcher: None,
        timeout: 10,
        watched: true,
    },
    Hook {
        event: "StopFailure",
        subcommand: "stop-hook",
        matcher: None,
        timeout: 10,
        watched: true,
    },
    Hook {
        event: "SessionStart",
        subcommand: "session-hook",
        matcher: None,
        timeout: 10,
        watched: true,
    },
    // Runs before every Bash call in every session, so it must stay quick.
    Hook {
        event: "PreToolUse",
        subcommand: "tool-hook",
        matcher: Some("Bash"),
        timeout: 5,
        watched: false,
    },
    // Runs on every prompt in every session, so it must stay quick too.
    Hook {
        event: "UserPromptSubmit",
        subcommand: "prompt-hook",
        matcher: None,
        timeout: 5,
        watched: false,
    },
];

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written, after backing up the old file here if there was one.
    Changed {
        backup: Option<PathBuf>,
    },
    Unchanged,
}

/// `$CLAUDE_CONFIG_DIR/settings.json`, defaulting to `~/.claude/settings.json`.
pub fn default_settings() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// `$CLAUDE_CONFIG_DIR`, defaulting to `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    Ok(
        match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => {
                PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".claude")
            }
        },
    )
}

/// The hook's shell command. `|| true` because exit 2 blocks: a Stop hook
/// keeps Claude from stopping, a PreToolUse hook the tool call, a
/// UserPromptSubmit hook the prompt, and a missing or old binary must never
/// do that. tool-hook blocks with JSON instead.
pub fn hook_command(exe: &Path, subcommand: &str) -> Result<String> {
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow!("{} is not UTF-8", exe.display()))?;
    Ok(format!(
        "'{}' {subcommand} || true",
        path.replace('\'', r"'\''")
    ))
}

/// Adds or updates loopherd's hooks, calling `exe` from each.
pub fn install(settings: &Path, exe: &Path, now: SystemTime) -> Result<Outcome> {
    edit(settings, now, |root| {
        let hooks = object_at(root, "hooks", "settings")?;
        for hook in &HOOKS {
            let ours = json!({
                "type": "command",
                "command": hook_command(exe, hook.subcommand)?,
                "timeout": hook.timeout,
            });
            let groups = array_at(hooks, hook.event)?;
            // Ours under another matcher, or none, moves to the one right group.
            if let Some(matcher) = hook.matcher {
                drop_ours(groups, hook.subcommand, |g| matcher_of(g) != Some(matcher));
            }
            if !keep_first_of_ours(groups, &ours, hook.subcommand, hook.matcher) {
                let mut group = Map::new();
                if let Some(matcher) = hook.matcher {
                    group.insert("matcher".into(), matcher.into());
                }
                group.insert("hooks".into(), json!([ours]));
                groups.push(Value::Object(group));
            }
        }
        Ok(())
    })
}

/// Takes loopherd's hooks out, and any group, event or `hooks` key that held
/// only them.
pub fn remove(settings: &Path, now: SystemTime) -> Result<Outcome> {
    edit(settings, now, |root| {
        let Some(hooks) = root.get_mut("hooks") else {
            return Ok(());
        };
        let hooks = hooks
            .as_object_mut()
            .ok_or_else(|| anyhow!("hooks is not an object"))?;
        let mut dropped = false;
        for hook in &HOOKS {
            let Some(groups) = hooks.get_mut(hook.event) else {
                continue;
            };
            let groups = groups
                .as_array_mut()
                .ok_or_else(|| anyhow!("hooks.{} is not an array", hook.event))?;
            if drop_ours(groups, hook.subcommand, |_| true) {
                dropped = true;
                if groups.is_empty() {
                    hooks.remove(hook.event);
                }
            }
        }
        if dropped && hooks.is_empty() {
            root.remove("hooks");
        }
        Ok(())
    })
}

/// Whether `settings` has the hooks `watch` needs: Stop, StopFailure and
/// SessionStart. The PreToolUse hook only guards leads, and the
/// UserPromptSubmit hook only clears pane labels.
pub fn installed(settings: &Path) -> Result<bool> {
    let bytes = match fs::read(settings) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("read {}", settings.display())),
    };
    let root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", settings.display()))?;
    let has_ours = |event: &str, subcommand: &str| {
        root.pointer(&format!("/hooks/{event}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
            .flatten()
            .any(|hook| is_ours(hook, subcommand))
    };
    Ok(HOOKS
        .iter()
        .filter(|h| h.watched)
        .all(|h| has_ours(h.event, h.subcommand)))
}

/// Applies `change` to the settings object and writes it back only if it
/// changed, backing up the old file first. Changes nothing on any error.
fn edit(
    settings: &Path,
    now: SystemTime,
    change: impl FnOnce(&mut Map<String, Value>) -> Result<()>,
) -> Result<Outcome> {
    let settings = through_symlinks(settings)?;
    let settings = settings.as_path();
    let old = match fs::read(settings) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", settings.display())),
    };
    let before = match &old {
        Some(bytes) => serde_json::from_slice(bytes)
            .with_context(|| format!("parse {}; left it unchanged", settings.display()))?,
        None => Value::Object(Map::new()),
    };
    let Value::Object(mut root) = before.clone() else {
        bail!(
            "{} is not a JSON object; left it unchanged",
            settings.display()
        );
    };
    change(&mut root).with_context(|| format!("{}; left it unchanged", settings.display()))?;
    let after = Value::Object(root);
    if after == before {
        return Ok(Outcome::Unchanged);
    }
    let backup = match &old {
        Some(bytes) => Some(back_up(settings, bytes, now)?),
        None => None,
    };
    let mut bytes = serde_json::to_vec_pretty(&after)?;
    bytes.push(b'\n');
    write_atomically(settings, &bytes)?;
    Ok(Outcome::Changed { backup })
}

fn back_up(settings: &Path, bytes: &[u8], now: SystemTime) -> Result<PathBuf> {
    let mut name = settings.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.bak", humantime::format_rfc3339_nanos(now)));
    let backup = settings.with_file_name(name);
    // Settings can hold API keys in `env`; the copy keeps the original's mode.
    let mode = fs::metadata(settings)?.permissions().mode();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&backup)
        .with_context(|| format!("back up to {}", backup.display()))?;
    fs::set_permissions(&backup, fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)
        .with_context(|| format!("back up to {}", backup.display()))?;
    Ok(backup)
}

/// The file a settings path names, so a symlink (e.g. into dotfiles) is
/// written through rather than replaced, even when its target doesn't exist yet.
fn through_symlinks(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&path)
                    .with_context(|| format!("read link {}", path.display()))?;
                path = path.parent().unwrap_or(Path::new("/")).join(target);
            }
            _ => return Ok(path),
        }
    }
    bail!("{}: too many levels of symlinks", path.display())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("settings path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    // Mode set before any byte lands, so a private file is never briefly readable.
    let mode = fs::metadata(path).map_or(0o644, |m| m.permissions().mode());
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

fn object_at<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a mut Map<String, Value>> {
    parent
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{key} in the {what} is not an object"))
}

fn array_at<'a>(hooks: &'a mut Map<String, Value>, event: &str) -> Result<&'a mut Vec<Value>> {
    hooks
        .entry(event)
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("hooks.{event} is not an array"))
}

/// Sets the first of loopherd's hooks in `groups` to `ours` and drops any
/// others; false when there was none.
fn keep_first_of_ours(
    groups: &mut Vec<Value>,
    ours: &Value,
    subcommand: &str,
    matcher: Option<&str>,
) -> bool {
    let mut found = false;
    for group in groups.iter_mut() {
        let Some(hooks) = hooks_of(group, matcher) else {
            continue;
        };
        let before = hooks.len();
        hooks.retain_mut(|hook| {
            if !is_ours(hook, subcommand) {
                return true;
            }
            if found {
                return false;
            }
            found = true;
            *hook = ours.clone();
            true
        });
        if hooks.is_empty() && before > 0 {
            // Held only a duplicate of ours; drop it after the loop.
            *group = Value::Null;
        }
    }
    groups.retain(|g| !g.is_null());
    found
}

fn matcher_of(group: &Value) -> Option<&str> {
    group.get("matcher").and_then(Value::as_str)
}

/// A group's hooks, unless the hook needs a `matcher` the group lacks.
fn hooks_of<'a>(group: &'a mut Value, matcher: Option<&str>) -> Option<&'a mut Vec<Value>> {
    if matcher.is_some_and(|m| matcher_of(group) != Some(m)) {
        return None;
    }
    group.get_mut("hooks").and_then(Value::as_array_mut)
}

/// Drops loopherd's hooks from the groups `from` picks, and groups left
/// empty; true if any was dropped.
fn drop_ours(groups: &mut Vec<Value>, subcommand: &str, from: impl Fn(&Value) -> bool) -> bool {
    let mut dropped = false;
    for group in groups.iter_mut() {
        if !from(group) {
            continue;
        }
        if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hooks.len();
            hooks.retain(|hook| !is_ours(hook, subcommand));
            if hooks.len() != before {
                dropped = true;
                if hooks.is_empty() {
                    *group = Value::Null;
                }
            }
        }
    }
    groups.retain(|g| !g.is_null());
    dropped
}

/// A command [`hook_command`] built, for a binary named `loopherd` anywhere;
/// also without `|| true`, as tool-hook was first installed.
fn is_ours(hook: &Value, subcommand: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .map(|c| c.strip_suffix(" || true").unwrap_or(c))
        .and_then(|c| c.strip_suffix(&format!(" {subcommand}")))
        .and_then(unquote)
        .is_some_and(|path| {
            Path::new(&path)
                .file_name()
                .is_some_and(|n| n == "loopherd")
        })
}

/// The word a single-quoted shell string spells, `'\''` escapes included.
fn unquote(quoted: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = quoted;
    loop {
        let body = rest.strip_prefix('\'')?;
        let end = body.find('\'')?;
        out.push_str(&body[..end]);
        rest = &body[end + 1..];
        match rest.strip_prefix(r"\'") {
            Some(after) => {
                out.push('\'');
                rest = after;
            }
            None => break,
        }
    }
    rest.is_empty().then_some(out)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const EXE: &str = "/home/me/.local/bin/loopherd";

    fn now() -> SystemTime {
        humantime::parse_rfc3339("2026-11-02T09:00:00Z").unwrap()
    }

    fn later() -> SystemTime {
        humantime::parse_rfc3339("2026-11-02T10:00:00Z").unwrap()
    }

    fn ours() -> Value {
        ours_running("stop-hook")
    }

    fn ours_running(subcommand: &str) -> Value {
        json!({"type": "command", "command": format!("'{EXE}' {subcommand} || true"), "timeout": 10})
    }

    fn tool_hook() -> Value {
        json!({"type": "command", "command": format!("'{EXE}' tool-hook || true"), "timeout": 5})
    }

    #[test]
    fn adds_a_pre_tool_use_hook_on_bash() {
        let user = r#"{"hooks": {"PreToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "lint"}]}]}}"#;
        let (_dir, path) = settings_with(user);

        install(&path, Path::new(EXE), now()).unwrap();

        let groups = read(&path)["hooks"]["PreToolUse"].clone();
        assert_eq!(
            groups,
            json!([
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "lint"}]},
                {"matcher": "Bash", "hooks": [tool_hook()]},
            ])
        );
        assert!(!groups.to_string().contains("onFailure"));
        assert_eq!(
            install(&path, Path::new(EXE), later()).unwrap(),
            Outcome::Unchanged
        );
        remove(&path, later()).unwrap();
        assert_eq!(read(&path), read_str(user));
    }

    #[test]
    fn a_tool_hook_under_another_matcher_or_none_moves_to_the_bash_group() {
        let theirs = json!({"type": "command", "command": "lint"});
        let stray = json!({"type": "command", "command": "'/old/loopherd' tool-hook || true"});
        let user = json!({"hooks": {"PreToolUse": [
            {"matcher": "Edit", "hooks": [theirs.clone(), stray.clone()]},
            {"hooks": [stray.clone()]},
            {"matcher": "Bash|Edit", "hooks": [stray]},
        ]}});
        let (_dir, path) = settings_with(&user.to_string());

        install(&path, Path::new(EXE), now()).unwrap();

        assert_eq!(
            read(&path)["hooks"]["PreToolUse"],
            json!([
                {"matcher": "Edit", "hooks": [theirs.clone()]},
                {"matcher": "Bash", "hooks": [tool_hook()]},
            ])
        );
        assert_eq!(
            install(&path, Path::new(EXE), later()).unwrap(),
            Outcome::Unchanged
        );
    }

    #[test]
    fn remove_takes_out_a_tool_hook_under_any_matcher() {
        let theirs = json!({"type": "command", "command": "lint"});
        let stray = json!({"type": "command", "command": "'/old/loopherd' tool-hook || true"});
        let user = json!({"hooks": {"PreToolUse": [
            {"matcher": "Edit", "hooks": [theirs.clone(), stray.clone()]},
            {"hooks": [stray.clone()]},
            {"matcher": "Bash", "hooks": [stray]},
        ]}});
        let (_dir, path) = settings_with(&user.to_string());

        remove(&path, now()).unwrap();

        assert_eq!(
            read(&path),
            json!({"hooks": {"PreToolUse": [{"matcher": "Edit", "hooks": [theirs]}]}})
        );
    }

    #[test]
    fn an_old_tool_hook_without_or_true_is_replaced() {
        let old = json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "'/old/loopherd' tool-hook"}]},
        ]}});
        let (_dir, path) = settings_with(&old.to_string());

        install(&path, Path::new(EXE), now()).unwrap();

        assert_eq!(
            read(&path)["hooks"]["PreToolUse"],
            json!([{"matcher": "Bash", "hooks": [tool_hook()]}])
        );
    }

    fn settings_with(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    const USER_SETTINGS: &str = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}],
    "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": "herdr-agent-state.sh"}]}]
  }
}"#;

    #[test]
    fn adds_stop_and_stop_failure_hooks_beside_the_users_own() {
        let (_dir, path) = settings_with(USER_SETTINGS);

        install(&path, Path::new(EXE), now()).unwrap();

        let after = read(&path);
        assert_eq!(after["model"], "opus");
        assert_eq!(
            after["hooks"]["Stop"],
            json!([
                {"hooks": [{"type": "command", "command": "notify-send done"}]},
                {"hooks": [ours()]},
            ])
        );
        assert_eq!(after["hooks"]["StopFailure"], json!([{"hooks": [ours()]}]));
        let mut session = read_str(USER_SETTINGS)["hooks"]["SessionStart"].clone();
        session
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks": [ours_running("session-hook")]}));
        assert_eq!(after["hooks"]["SessionStart"], session);
    }

    fn read_str(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn keeps_the_users_key_order() {
        let (_dir, path) = settings_with(r#"{"zeta": 1, "alpha": 2}"#);
        install(&path, Path::new(EXE), now()).unwrap();
        let keys: Vec<String> = read(&path).as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "alpha", "hooks"]);
    }

    #[test]
    fn backs_up_the_old_file_before_changing_it() {
        let (dir, path) = settings_with(USER_SETTINGS);

        let outcome = install(&path, Path::new(EXE), now()).unwrap();

        let backup = dir
            .path()
            .join("settings.json.2026-11-02T09:00:00.000000000Z.bak");
        assert_eq!(
            outcome,
            Outcome::Changed {
                backup: Some(backup.clone())
            }
        );
        assert_eq!(fs::read_to_string(backup).unwrap(), USER_SETTINGS);
    }

    #[test]
    fn installs_a_session_start_hook_and_remove_takes_it_out() {
        let (_dir, path) = settings_with(USER_SETTINGS);
        install(&path, Path::new(EXE), now()).unwrap();
        let groups = read(&path)["hooks"]["SessionStart"].clone();
        assert_eq!(
            groups.as_array().unwrap().last().unwrap(),
            &json!({"hooks": [ours_running("session-hook")]})
        );

        remove(&path, later()).unwrap();
        assert_eq!(
            read(&path)["hooks"]["SessionStart"],
            read_str(USER_SETTINGS)["hooks"]["SessionStart"]
        );
    }

    #[test]
    fn installs_a_quick_user_prompt_submit_hook_that_watch_does_not_need() {
        let (_dir, path) = settings_with(USER_SETTINGS);
        install(&path, Path::new(EXE), now()).unwrap();
        let prompt_hook = json!({"type": "command", "command": format!("'{EXE}' prompt-hook || true"), "timeout": 5});
        assert_eq!(
            read(&path)["hooks"]["UserPromptSubmit"],
            json!([{"hooks": [prompt_hook]}])
        );

        let mut no_prompt_hook = read(&path);
        no_prompt_hook["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("UserPromptSubmit");
        fs::write(&path, no_prompt_hook.to_string()).unwrap();
        assert!(installed(&path).unwrap());

        remove(&path, later()).unwrap();
        assert!(read(&path)["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn installed_needs_ours_on_the_three_watched_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(!installed(&path).unwrap());

        fs::write(&path, USER_SETTINGS).unwrap();
        assert!(!installed(&path).unwrap());

        install(&path, Path::new("/opt/bin/loopherd"), now()).unwrap();
        assert!(installed(&path).unwrap());

        let mut no_tool_hook = read(&path);
        no_tool_hook["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("PreToolUse");
        fs::write(&path, no_tool_hook.to_string()).unwrap();
        assert!(
            installed(&path).unwrap(),
            "watch doesn't need the tool hook"
        );

        let mut half = read(&path);
        half["hooks"].as_object_mut().unwrap().remove("StopFailure");
        fs::write(&path, half.to_string()).unwrap();
        assert!(!installed(&path).unwrap());

        let mut no_session = read(&path);
        no_session["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        fs::write(&path, no_session.to_string()).unwrap();
        assert!(!installed(&path).unwrap());

        fs::write(&path, "not json").unwrap();
        assert!(installed(&path).is_err());
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let (dir, path) = settings_with(USER_SETTINGS);
        install(&path, Path::new(EXE), now()).unwrap();
        let first = fs::read(&path).unwrap();
        let listed = files(dir.path());

        assert_eq!(
            install(&path, Path::new(EXE), later()).unwrap(),
            Outcome::Unchanged
        );

        assert_eq!(fs::read(&path).unwrap(), first);
        assert_eq!(files(dir.path()), listed);
    }

    #[test]
    fn a_moved_binary_updates_its_entry_in_place() {
        let (_dir, path) = settings_with(USER_SETTINGS);
        install(&path, Path::new("/old/bin/loopherd"), now()).unwrap();

        install(&path, Path::new(EXE), later()).unwrap();

        let after = read(&path);
        assert_eq!(after["hooks"]["Stop"][1], json!({"hooks": [ours()]}));
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(after["hooks"]["StopFailure"], json!([{"hooks": [ours()]}]));
    }

    #[test]
    fn a_duplicate_entry_is_dropped() {
        let doubled = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "'/a/loopherd' stop-hook || true"}]},
            {"hooks": [{"type": "command", "command": "'/b/loopherd' stop-hook || true"}]},
        ]}});
        let (_dir, path) = settings_with(&doubled.to_string());

        install(&path, Path::new(EXE), now()).unwrap();

        assert_eq!(read(&path)["hooks"]["Stop"], json!([{"hooks": [ours()]}]));
    }

    #[test]
    fn a_missing_file_is_created_without_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let outcome = install(&path, Path::new(EXE), now()).unwrap();

        assert_eq!(outcome, Outcome::Changed { backup: None });
        assert_eq!(
            read(&path),
            json!({"hooks": {
                "Stop": [{"hooks": [ours()]}],
                "StopFailure": [{"hooks": [ours()]}],
                "SessionStart": [{"hooks": [ours_running("session-hook")]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [tool_hook()]}],
                "UserPromptSubmit": [{"hooks": [
                    json!({"type": "command", "command": format!("'{EXE}' prompt-hook || true"), "timeout": 5})
                ]}],
            }})
        );
        assert_eq!(files(dir.path()), ["settings.json"]);
    }

    #[test]
    fn malformed_or_non_object_json_is_refused_and_left_unchanged() {
        for contents in [
            "{\"hooks\": ",
            "",
            "[]",
            "\"settings\"",
            r#"{"hooks": []}"#,
            r#"{"hooks": {"Stop": {}}}"#,
        ] {
            let (dir, path) = settings_with(contents);

            let err = install(&path, Path::new(EXE), now()).unwrap_err();

            assert!(
                format!("{err:#}").contains("left it unchanged"),
                "{contents}: {err:#}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            assert_eq!(files(dir.path()), ["settings.json"], "{contents}");
            assert!(remove(&path, now()).is_err(), "{contents}");
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[test]
    fn remove_restores_the_file_minus_loopherds_entries() {
        for original in [USER_SETTINGS, r#"{"model": "opus"}"#] {
            let (_dir, path) = settings_with(original);
            install(&path, Path::new(EXE), now()).unwrap();

            assert!(matches!(
                remove(&path, later()).unwrap(),
                Outcome::Changed { .. }
            ));

            assert_eq!(read(&path), read_str(original), "{original}");
        }
    }

    #[test]
    fn remove_with_nothing_of_ours_changes_nothing() {
        let (dir, path) = settings_with(USER_SETTINGS);
        assert_eq!(remove(&path, now()).unwrap(), Outcome::Unchanged);
        assert_eq!(fs::read_to_string(&path).unwrap(), USER_SETTINGS);
        assert_eq!(files(dir.path()), ["settings.json"]);
    }

    #[test]
    fn writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles.json");
        fs::write(&target, "{}").unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        install(&link, Path::new(EXE), now()).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(read(&target)["hooks"]["Stop"].is_array());
    }

    #[test]
    fn the_command_round_trips_paths_with_spaces_and_quotes() {
        for exe in ["/a b/loopherd", "/it's/loopherd"] {
            let command = hook_command(Path::new(exe), "stop-hook").unwrap();
            for command in [command.as_str(), command.trim_end_matches(" || true")] {
                assert!(
                    is_ours(&json!({"command": command}), "stop-hook"),
                    "{command}"
                );
            }
        }
        assert!(!is_ours(
            &json!({"command": "'/bin/other' stop-hook || true"}),
            "stop-hook"
        ));
        assert!(!is_ours(
            &json!({"command": "notify-send done"}),
            "stop-hook"
        ));
        let session = hook_command(Path::new(EXE), "session-hook").unwrap();
        assert!(!is_ours(&json!({"command": session}), "stop-hook"));
    }

    fn run_hook(exe: &Path) -> std::process::ExitStatus {
        Command::new("sh")
            .args(["-c", &hook_command(exe, "stop-hook").unwrap()])
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    #[test]
    fn the_hook_command_exits_0_whatever_loopherd_does() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("a dir's/bin");
        fs::create_dir_all(&bin).unwrap();
        let failing = bin.join("loopherd");
        crate::test_fakes::executable(&failing, "#!/bin/sh\necho \"usage: $*\" >&2\nexit 2\n");

        assert_eq!(run_hook(&failing).code(), Some(0), "usage error");
        assert_eq!(
            run_hook(&bin.join("missing/loopherd")).code(),
            Some(0),
            "missing binary"
        );
    }

    #[test]
    fn the_backup_keeps_a_private_files_mode() {
        let (dir, path) = settings_with(r#"{"env": {"ANTHROPIC_API_KEY": "x"}}"#);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let Outcome::Changed {
            backup: Some(backup),
        } = install(&path, Path::new(EXE), now()).unwrap()
        else {
            panic!("no backup");
        };

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&backup), 0o600);
        assert_eq!(mode(&path), 0o600);
        drop(dir);
    }

    #[test]
    fn remove_leaves_a_missing_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(remove(&path, now()).unwrap(), Outcome::Unchanged);
        assert!(!path.exists());
    }

    #[test]
    fn the_users_empty_keys_and_groups_survive_install_and_remove() {
        for original in [
            r#"{"hooks": {}}"#,
            r#"{"hooks": {"Stop": []}}"#,
            r#"{"hooks": {"Stop": [{"matcher": "", "hooks": []}]}}"#,
        ] {
            let (_dir, path) = settings_with(original);
            assert_eq!(
                remove(&path, now()).unwrap(),
                Outcome::Unchanged,
                "{original}"
            );
        }
        // An empty key loopherd's entry was later added to can't be told from
        // one loopherd created, so only a non-empty user group round-trips.
        let original = r#"{"hooks": {"Stop": [{"matcher": "", "hooks": []}]}}"#;
        let (_dir, path) = settings_with(original);
        install(&path, Path::new(EXE), now()).unwrap();
        assert_eq!(read(&path)["hooks"]["Stop"].as_array().unwrap().len(), 2);
        remove(&path, later()).unwrap();
        assert_eq!(read(&path), read_str(original));
    }

    #[test]
    fn writes_through_a_dangling_symlink_to_its_target() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dotfiles")).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &link).unwrap();

        install(&link, Path::new(EXE), now()).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(read(&dir.path().join("dotfiles/settings.json"))["hooks"]["Stop"].is_array());
    }
}
