//! `loopherd doctor`: whether this machine's loopherd install and herdr are
//! sound. It only reads, and names the command that fixes each problem.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::herdr::Herdr;

/// What the install checks read, gathered by the caller.
pub struct Install {
    pub settings: PathBuf,
    /// The settings file came from `--settings`, so each fix names it too.
    pub settings_given: bool,
    pub pane_label: bool,
    /// The `loopherd` a shell finds on PATH.
    pub on_path: Option<PathBuf>,
    pub running: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Failure,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

/// The oldest herdr loopherd is tested against.
pub const MIN_HERDR: &str = "0.9.5";

pub fn checks(install: &Install, herdr: &dyn Herdr) -> Vec<Check> {
    let mut checks = hook_checks(install);
    checks.push(running_binary(install));
    checks.extend(herdr_checks(herdr));
    checks
}

fn hook_checks(install: &Install) -> Vec<Check> {
    let settings = install.settings.display();
    let install_hook = match install.settings_given {
        true => format!("loopherd install-hook --settings {settings}"),
        false => "loopherd install-hook".to_string(),
    };
    let hooks = match crate::install_hook::hooks(&install.settings) {
        Ok(hooks) => hooks,
        Err(e) => {
            return vec![failure(
                "settings",
                format!("{e:#}").trim_end().to_string(),
                format!("fix {settings} by hand, then {install_hook}"),
            )];
        }
    };
    let mut checks: Vec<Check> = hooks
        .iter()
        .map(|hook| {
            let name = format!("{} hook", hook.event);
            match (hook.binaries.is_empty(), hook.event) {
                (false, _) => ok(&name, format!("in {settings}")),
                (true, "UserPromptSubmit") if !install.pane_label => Check {
                    status: Status::Warning,
                    ..failure(&name, format!("not in {settings}"), install_hook.clone())
                },
                (true, _) => failure(&name, format!("not in {settings}"), install_hook.clone()),
            }
        })
        .collect();
    let called: Vec<&PathBuf> = hooks.iter().flat_map(|h| &h.binaries).collect();
    checks.push(match &install.on_path {
        None => failure(
            "hook binary",
            "no loopherd on PATH".into(),
            format!("install loopherd on PATH, then {install_hook}"),
        ),
        Some(on_path) => match called.iter().find(|b| !same_file(b, on_path)) {
            Some(other) => failure(
                "hook binary",
                format!(
                    "hooks call {}, not the loopherd on PATH ({})",
                    other.display(),
                    on_path.display()
                ),
                install_hook,
            ),
            None if called.is_empty() => ok("hook binary", "no hooks to check".into()),
            None => ok("hook binary", format!("hooks call {}", on_path.display())),
        },
    });
    checks
}

fn running_binary(install: &Install) -> Check {
    let running = install.running.display();
    match &install.on_path {
        Some(on_path) if !same_file(&install.running, on_path) => Check {
            name: "running binary".into(),
            status: Status::Warning,
            detail: format!(
                "this is {running}, not the loopherd on PATH ({})",
                on_path.display()
            ),
            fix: Some("run the loopherd on PATH".into()),
        },
        _ => ok("running binary", running.to_string()),
    }
}

fn herdr_checks(herdr: &dyn Herdr) -> Vec<Check> {
    let socket = match herdr.socket() {
        Some(socket) => socket.display().to_string(),
        None => "herdr's default socket".into(),
    };
    let version = match herdr.version() {
        Ok(Some(version)) => version,
        Ok(None) => {
            return vec![failure(
                "herdr",
                format!("no herdr server answers on {socket}"),
                "herdr".into(),
            )];
        }
        Err(e) => {
            return vec![failure(
                "herdr",
                format!("{e:#}").trim_end().to_string(),
                "install herdr, then herdr".into(),
            )];
        }
    };
    let upgrade = format!("upgrade herdr to {MIN_HERDR} or later");
    let check = match numbers(&version) {
        None => failure(
            "herdr version",
            format!("can't read herdr's version {version:?}"),
            upgrade,
        ),
        Some(v) if Some(v) < numbers(MIN_HERDR) => failure(
            "herdr version",
            format!("{version} is older than {MIN_HERDR}"),
            upgrade,
        ),
        Some(_) => ok("herdr version", version),
    };
    vec![ok("herdr", format!("answers on {socket}")), check]
}

/// `major.minor.patch` as numbers, ignoring any pre-release or build suffix.
fn numbers(version: &str) -> Option<[u64; 3]> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(numbers)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn ok(name: &str, detail: String) -> Check {
    Check {
        name: name.into(),
        status: Status::Ok,
        detail,
        fix: None,
    }
}

fn failure(name: &str, detail: String, fix: String) -> Check {
    Check {
        name: name.into(),
        status: Status::Failure,
        detail,
        fix: Some(fix),
    }
}

fn count(checks: &[Check], status: Status) -> usize {
    checks.iter().filter(|c| c.status == status).count()
}

/// Problems with their fixes, or every check when `verbose`, then the summary line.
pub fn text(checks: &[Check], verbose: bool) -> String {
    let mut out = String::new();
    for check in checks.iter().filter(|c| verbose || c.status != Status::Ok) {
        let status = match check.status {
            Status::Ok => "ok",
            Status::Warning => "warning",
            Status::Failure => "failure",
        };
        out += &format!("{status} {}: {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            out += &format!("; fix: {fix}");
        }
        out.push('\n');
    }
    out + &format!(
        "doctor: {} ok, {} warnings, {} failures\n",
        count(checks, Status::Ok),
        count(checks, Status::Warning),
        count(checks, Status::Failure),
    )
}

pub fn json(checks: &[Check]) -> String {
    serde_json::json!({
        "checks": checks,
        "ok": count(checks, Status::Ok),
        "warnings": count(checks, Status::Warning),
        "failures": count(checks, Status::Failure),
    })
    .to_string()
}

/// 0 when clean or only warnings, 1 on any failure.
pub fn exit_code(checks: &[Check]) -> i32 {
    match count(checks, Status::Failure) {
        0 => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use serde_json::{Value, json};

    const EVENTS: [(&str, &str); 5] = [
        ("Stop", "stop-hook"),
        ("StopFailure", "stop-hook"),
        ("SessionStart", "session-hook"),
        ("PreToolUse", "tool-hook"),
        ("UserPromptSubmit", "prompt-hook"),
    ];

    struct Machine {
        dir: tempfile::TempDir,
        install: Install,
    }

    /// A settings file whose hooks for `events` call `exe`, with `exe` both
    /// on PATH and running.
    fn machine(events: &[&str]) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/loopherd");
        let settings = dir.path().join("settings.json");
        write_hooks(&settings, &exe, events);
        Machine {
            install: Install {
                settings,
                settings_given: false,
                pane_label: false,
                on_path: Some(exe.clone()),
                running: exe,
            },
            dir,
        }
    }

    fn write_hooks(settings: &Path, exe: &Path, events: &[&str]) {
        let mut hooks = serde_json::Map::new();
        for (event, subcommand) in EVENTS.iter().filter(|(e, _)| events.contains(e)) {
            let command = crate::install_hook::hook_command(exe, subcommand).unwrap();
            hooks.insert(
                event.to_string(),
                json!([{"hooks": [{"type": "command", "command": command}]}]),
            );
        }
        let root = json!({ "hooks": hooks });
        std::fs::write(settings, serde_json::to_vec(&root).unwrap()).unwrap();
    }

    const ALL: [&str; 5] = [
        "Stop",
        "StopFailure",
        "SessionStart",
        "PreToolUse",
        "UserPromptSubmit",
    ];

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[])
    }

    fn run(install: &Install, herdr: &FakeHerdr) -> (String, i32) {
        let checks = checks(install, herdr);
        (text(&checks, false), exit_code(&checks))
    }

    #[test]
    fn a_sound_install_prints_only_the_summary_and_exits_0() {
        let m = machine(&ALL);
        assert_eq!(
            run(&m.install, &herdr()),
            ("doctor: 9 ok, 0 warnings, 0 failures\n".into(), 0)
        );
    }

    #[test]
    fn a_missing_watched_or_tool_hook_fails_naming_install_hook() {
        for missing in ["Stop", "StopFailure", "SessionStart", "PreToolUse"] {
            let events: Vec<&str> = ALL.into_iter().filter(|e| *e != missing).collect();
            let m = machine(&events);
            let (out, code) = run(&m.install, &herdr());
            assert_eq!(
                out,
                format!(
                    "failure {missing} hook: not in {}; fix: loopherd install-hook\n\
                     doctor: 8 ok, 0 warnings, 1 failures\n",
                    m.install.settings.display()
                ),
            );
            assert_eq!(code, 1);
        }
    }

    #[test]
    fn a_missing_prompt_hook_warns_unless_pane_label_is_on() {
        let mut m = machine(&ALL[..4]);
        let line = format!(
            "UserPromptSubmit hook: not in {}; fix: loopherd install-hook\n",
            m.install.settings.display()
        );
        assert_eq!(
            run(&m.install, &herdr()),
            (
                format!("warning {line}doctor: 8 ok, 1 warnings, 0 failures\n"),
                0
            )
        );
        m.install.pane_label = true;
        assert_eq!(
            run(&m.install, &herdr()),
            (
                format!("failure {line}doctor: 8 ok, 0 warnings, 1 failures\n"),
                1
            )
        );
    }

    #[test]
    fn a_settings_flag_is_repeated_in_the_fix() {
        let mut m = machine(&ALL[..3]);
        m.install.settings_given = true;
        let (out, _) = run(&m.install, &herdr());
        assert!(
            out.contains(&format!(
                "fix: loopherd install-hook --settings {}\n",
                m.install.settings.display()
            )),
            "{out}"
        );
    }

    #[test]
    fn a_missing_settings_file_fails_every_hook() {
        let mut m = machine(&ALL);
        m.install.settings = m.dir.path().join("absent.json");
        let (out, code) = run(&m.install, &herdr());
        assert!(
            out.ends_with("doctor: 4 ok, 1 warnings, 4 failures\n"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn unparseable_settings_fail_once_naming_the_file() {
        let m = machine(&ALL);
        std::fs::write(&m.install.settings, "{ not json").unwrap();
        let (out, code) = run(&m.install, &herdr());
        let first = out.lines().next().unwrap();
        assert!(
            first.starts_with(&format!(
                "failure settings: parse {}",
                m.install.settings.display()
            )),
            "{out}"
        );
        assert!(
            out.ends_with("doctor: 3 ok, 0 warnings, 1 failures\n"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn hooks_calling_another_binary_than_paths_fail() {
        let m = machine(&ALL);
        let old = m.dir.path().join("old/loopherd");
        write_hooks(&m.install.settings, &old, &ALL[..1]);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        let path_exe = m.install.on_path.clone().unwrap();
        let ours = |sub: &str| crate::install_hook::hook_command(&path_exe, sub).unwrap();
        for (event, sub) in &EVENTS[1..] {
            settings["hooks"][event] =
                json!([{"hooks": [{"type": "command", "command": ours(sub)}]}]);
        }
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m.install, &herdr());
        assert_eq!(
            out,
            format!(
                "failure hook binary: hooks call {}, not the loopherd on PATH ({}); \
                 fix: loopherd install-hook\n\
                 doctor: 8 ok, 0 warnings, 1 failures\n",
                old.display(),
                path_exe.display()
            )
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn no_loopherd_on_path_fails() {
        let mut m = machine(&ALL);
        m.install.on_path = None;
        let (out, code) = run(&m.install, &herdr());
        assert!(
            out.starts_with(
                "failure hook binary: no loopherd on PATH; \
                 fix: install loopherd on PATH, then loopherd install-hook\n"
            ),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn running_another_binary_than_paths_warns() {
        let mut m = machine(&ALL);
        let dev = m.dir.path().join("target/debug/loopherd");
        m.install.running = dev.clone();
        let (out, code) = run(&m.install, &herdr());
        assert_eq!(
            out,
            format!(
                "warning running binary: this is {}, not the loopherd on PATH ({}); \
                 fix: run the loopherd on PATH\n\
                 doctor: 8 ok, 1 warnings, 0 failures\n",
                dev.display(),
                m.install.on_path.as_ref().unwrap().display()
            )
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn the_same_binary_through_a_symlink_is_paths() {
        let mut m = machine(&ALL);
        let real = m.install.running.clone();
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        let link = m.dir.path().join("loopherd");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        m.install.on_path = Some(link);
        assert_eq!(run(&m.install, &herdr()).1, 0);
        assert!(run(&m.install, &herdr()).0.starts_with("doctor: 9 ok"));
    }

    #[test]
    fn an_unreachable_herdr_fails_naming_its_socket() {
        let m = machine(&ALL);
        let herdr = herdr();
        herdr.set_unreachable(true);
        let (out, code) = run(&m.install, &herdr);
        assert_eq!(
            out,
            "failure herdr: no herdr server answers on /run/fake/herdr.sock; fix: herdr\n\
             doctor: 7 ok, 0 warnings, 1 failures\n"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_herdr_older_than_min_herdr_fails() {
        let m = machine(&ALL);
        for (version, ok) in [
            ("0.9.4", false),
            ("0.8.12", false),
            ("0.9.5", true),
            ("0.9.10", true),
            ("1.0.0-rc.1", true),
        ] {
            let mut herdr = herdr();
            herdr.version = version.into();
            let (out, code) = run(&m.install, &herdr);
            match ok {
                true => assert_eq!(code, 0, "{version}: {out}"),
                false => assert_eq!(
                    out,
                    format!(
                        "failure herdr version: {version} is older than {MIN_HERDR}; \
                         fix: upgrade herdr to {MIN_HERDR} or later\n\
                         doctor: 8 ok, 0 warnings, 1 failures\n"
                    )
                ),
            }
        }
    }

    #[test]
    fn verbose_lists_every_check() {
        let m = machine(&ALL);
        let checks = checks(&m.install, &herdr());
        let exe = m.install.running.display();
        let settings = m.install.settings.display();
        assert_eq!(
            text(&checks, true),
            format!(
                "ok Stop hook: in {settings}\n\
                 ok StopFailure hook: in {settings}\n\
                 ok SessionStart hook: in {settings}\n\
                 ok PreToolUse hook: in {settings}\n\
                 ok UserPromptSubmit hook: in {settings}\n\
                 ok hook binary: hooks call {exe}\n\
                 ok running binary: {exe}\n\
                 ok herdr: answers on /run/fake/herdr.sock\n\
                 ok herdr version: 0.9.5\n\
                 doctor: 9 ok, 0 warnings, 0 failures\n"
            )
        );
    }

    #[test]
    fn json_gives_every_check_and_the_counts() {
        let m = machine(&ALL[..4]);
        let checks = checks(&m.install, &herdr());
        let data: Value = serde_json::from_str(&json(&checks)).unwrap();
        assert_eq!(
            (&data["ok"], &data["warnings"], &data["failures"]),
            (&json!(8), &json!(1), &json!(0))
        );
        assert_eq!(data["checks"].as_array().unwrap().len(), 9);
        assert_eq!(
            data["checks"][4],
            json!({
                "name": "UserPromptSubmit hook",
                "status": "warning",
                "detail": format!("not in {}", m.install.settings.display()),
                "fix": "loopherd install-hook",
            })
        );
        assert_eq!(data["checks"][0]["fix"], Value::Null);
    }
}
