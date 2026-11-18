//! `loopherd doctor`: whether this machine's loopherd install and herdr are
//! sound. It only reads, and names the command that fixes each problem.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::herdr::{Herdr, Server};

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
        true => format!(
            "loopherd install-hook --settings {}",
            crate::shell::quote(&install.settings.to_string_lossy())
        ),
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
                (true, "UserPromptSubmit") if !install.pane_label => {
                    warning(&name, format!("not in {settings}"), install_hook.clone())
                }
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
        // A relative path resolves against whatever cwd Claude runs the hook in.
        Some(on_path) => match called
            .iter()
            .find(|b| !b.is_absolute() || !same_file(b, on_path))
        {
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
        Some(on_path) if !same_file(&install.running, on_path) => warning(
            "running binary",
            format!(
                "this is {running}, not the loopherd on PATH ({})",
                on_path.display()
            ),
            "run the loopherd on PATH".into(),
        ),
        _ => ok("running binary", running.to_string()),
    }
}

/// How long herdr's status probe may take; a wedged server never answers.
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

fn herdr_checks(herdr: &dyn Herdr) -> Vec<Check> {
    let on = |socket: Option<PathBuf>| match socket {
        Some(socket) => socket.display().to_string(),
        None => "herdr's default socket".into(),
    };
    let (socket, version) = match herdr.server(HERDR_TIMEOUT) {
        Ok(Server::Running { socket, version }) => (on(socket), version),
        Ok(Server::NotRunning { socket }) => {
            let detail = format!("no herdr server answers on {}", on(socket));
            return vec![failure("herdr", detail, "herdr".into())];
        }
        Ok(Server::NoAnswer { socket }) => {
            let detail = format!(
                "herdr didn't answer on {} within {}",
                on(socket),
                humantime::format_duration(HERDR_TIMEOUT)
            );
            return vec![failure("herdr", detail, "restart herdr's server".into())];
        }
        Ok(Server::Missing) => {
            return vec![failure(
                "herdr",
                "no herdr on PATH".into(),
                "install herdr".into(),
            )];
        }
        Err(e) => {
            let detail = format!("{e:#}").trim_end().to_string();
            return vec![failure("herdr", detail, "restart herdr's server".into())];
        }
    };
    let upgrade = format!("upgrade herdr to {MIN_HERDR} or later and restart its server");
    let check = match version {
        None => failure(
            "herdr version",
            "herdr's server didn't report a version".into(),
            upgrade,
        ),
        Some(version) => match numbers(&version) {
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
        },
    };
    vec![
        ok("herdr", format!("answers on {socket}")),
        check,
        plugin_check(herdr),
    ]
}

/// Only ever warns: loopherd works without its herdr plugin.
fn plugin_check(herdr: &dyn Herdr) -> Check {
    let ours = env!("CARGO_PKG_VERSION");
    let unavailable = "so its actions, popups and event hooks are unavailable";
    let plugin = match herdr.plugin("loopherd", HERDR_TIMEOUT) {
        Ok(Some(plugin)) => plugin,
        Ok(None) => {
            return warning(
                "herdr plugin",
                format!("loopherd's herdr plugin isn't installed, {unavailable}"),
                "herdr plugin install junderdo/loopherd/herdr-plugin".into(),
            );
        }
        Err(e) => {
            let detail = format!("can't list herdr's plugins: {e:#}");
            let fix = "run `herdr plugin list --plugin loopherd` to see why".into();
            return warning("herdr plugin", detail.trim_end().into(), fix);
        }
    };
    if !plugin.enabled {
        return warning(
            "herdr plugin",
            format!("loopherd's herdr plugin is disabled, {unavailable}"),
            "herdr plugin enable loopherd".into(),
        );
    }
    if plugin.version != ours {
        let newer = numbers(&plugin.version)
            .zip(numbers(ours))
            .is_some_and(|(plugin, ours)| plugin > ours);
        let fix = match &plugin.linked {
            _ if newer => format!("update loopherd to {}", plugin.version),
            Some(dir) => format!(
                "bring the linked plugin at {} to loopherd {ours}",
                dir.display()
            ),
            None => "herdr plugin update loopherd".into(),
        };
        let detail = format!("plugin {} differs from loopherd {ours}", plugin.version);
        return warning("herdr plugin", detail, fix);
    }
    let detail = match &plugin.linked {
        Some(dir) => format!("{ours}, enabled, linked from {}", dir.display()),
        None => format!("{ours}, enabled"),
    };
    ok("herdr plugin", detail)
}

/// How long each herd's `source` or `backlog` may run under `--trackers`.
pub const TRACKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs every herd's `source`, and `backlog` when set, from the herd's repo,
/// paused herds included. Reports each exit and task count, never the tasks.
pub fn tracker_checks(paths: &crate::herd::Paths, timeout: Duration) -> Vec<Check> {
    let names = match crate::herd::names(&paths.herds) {
        Ok(names) => names,
        Err(e) => {
            let detail = format!("{e:#}").trim_end().to_string();
            return vec![failure("trackers", detail, "loopherd herds".into())];
        }
    };
    let mut checks = Vec::new();
    for herd in names {
        let queue = match crate::herd::lead_checks(paths, &herd) {
            Ok((_, Some(queue))) => queue,
            Ok((_, None)) => continue,
            Err(e) => {
                let detail = format!("{e:#}").trim_end().to_string();
                checks.push(failure(
                    &format!("{herd} trackers"),
                    detail,
                    "loopherd herds".into(),
                ));
                continue;
            }
        };
        if !queue.repo.is_dir() {
            let repo = queue.repo.display();
            checks.push(failure(
                &format!("{herd} repo"),
                format!("{repo} isn't a directory, so its trackers can't run"),
                format!("loopherd herd set {herd} repo <path>"),
            ));
            continue;
        }
        let commands = [
            ("source", Some(&queue.source)),
            ("backlog", queue.backlog.as_ref()),
        ];
        for (key, command) in commands {
            if let Some(command) = command {
                checks.push(tracker(&herd, key, command, &queue.repo, timeout));
            }
        }
    }
    checks
}

fn tracker(herd: &str, key: &str, command: &str, repo: &Path, timeout: Duration) -> Check {
    let name = format!("{herd} {key}");
    let shown = match command.trim().split_once('\n') {
        Some((first, _)) => format!("{first} …"),
        None => command.trim().to_string(),
    };
    let fix =
        format!("fix {herd}'s queue.{key}, e.g. loopherd herd set {herd} queue.{key} <command>");
    let ran = crate::shell::run_within(
        Command::new("sh").arg("-c").arg(command).current_dir(repo),
        timeout,
    );
    let output = match ran {
        Ok(Some(output)) => output,
        Ok(None) => {
            let after = humantime::format_duration(timeout);
            return failure(&name, format!("{shown:?} timed out after {after}"), fix);
        }
        Err(e) => {
            let detail = format!("{shown:?} didn't run in {}: {e:#}", repo.display());
            return failure(&name, detail, fix);
        }
    };
    let exit = match output.status.code() {
        Some(code) => format!("exit {code}"),
        None => output.status.to_string(),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match stderr.trim().lines().last() {
            None => format!("{shown:?} failed ({exit})"),
            Some(last) => format!(
                "{shown:?} failed ({exit}): {}",
                crate::watch::printable(last)
            ),
        };
        return failure(&name, detail, fix);
    }
    let tasks = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count();
    ok(&name, format!("{exit}, {tasks} tasks"))
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

fn warning(name: &str, detail: String, fix: String) -> Check {
    Check {
        status: Status::Warning,
        ..failure(name, detail, fix)
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
    use crate::herdr::Plugin;
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
            hooks.insert(event.to_string(), group(event, &command));
        }
        let root = json!({ "hooks": hooks });
        std::fs::write(settings, serde_json::to_vec(&root).unwrap()).unwrap();
    }

    /// A settings group holding one hook running `command`, under Bash for PreToolUse.
    fn group(event: &str, command: &str) -> Value {
        let hooks = json!([{"type": "command", "command": command}]);
        match event {
            "PreToolUse" => json!([{"matcher": "Bash", "hooks": hooks}]),
            _ => json!([{ "hooks": hooks }]),
        }
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
            ("doctor: 10 ok, 0 warnings, 0 failures\n".into(), 0)
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
                     doctor: 9 ok, 0 warnings, 1 failures\n",
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
                format!("warning {line}doctor: 9 ok, 1 warnings, 0 failures\n"),
                0
            )
        );
        m.install.pane_label = true;
        assert_eq!(
            run(&m.install, &herdr()),
            (
                format!("failure {line}doctor: 9 ok, 0 warnings, 1 failures\n"),
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
                "fix: loopherd install-hook --settings '{}'\n",
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
            out.ends_with("doctor: 5 ok, 1 warnings, 4 failures\n"),
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
            out.ends_with("doctor: 4 ok, 0 warnings, 1 failures\n"),
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
            settings["hooks"][event] = group(event, &ours(sub));
        }
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m.install, &herdr());
        assert_eq!(
            out,
            format!(
                "failure hook binary: hooks call {}, not the loopherd on PATH ({}); \
                 fix: loopherd install-hook\n\
                 doctor: 9 ok, 0 warnings, 1 failures\n",
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
                 doctor: 9 ok, 1 warnings, 0 failures\n",
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
        assert!(run(&m.install, &herdr()).0.starts_with("doctor: 10 ok"));
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
                         fix: upgrade herdr to {MIN_HERDR} or later and restart its server\n\
                         doctor: 9 ok, 0 warnings, 1 failures\n"
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
                 ok herdr plugin: {VERSION}, enabled\n\
                 doctor: 10 ok, 0 warnings, 0 failures\n"
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
            (&json!(9), &json!(1), &json!(0))
        );
        assert_eq!(data["checks"].as_array().unwrap().len(), 10);
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

    #[test]
    fn a_settings_path_with_spaces_and_quotes_is_quoted_in_the_fix() {
        let mut m = machine(&ALL[..3]);
        m.install.settings = m.dir.path().join("a dir/it's.json");
        m.install.settings_given = true;
        let (out, _) = run(&m.install, &herdr());
        let quoted = format!("'{}/a dir/it'\\''s.json'", m.dir.path().display());
        assert!(
            out.contains(&format!("fix: loopherd install-hook --settings {quoted}\n")),
            "{out}"
        );
    }

    #[test]
    fn a_tool_hook_under_another_matcher_than_bash_is_missing() {
        let m = machine(&ALL);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        settings["hooks"]["PreToolUse"][0]["matcher"] = json!("Edit");
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m.install, &herdr());
        assert!(out.starts_with("failure PreToolUse hook: not in "), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn the_users_own_hooks_are_neither_ours_nor_a_binary_to_check() {
        let m = machine(&ALL[1..]);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        let theirs = json!({"type": "command", "command": "'/usr/bin/notify-send' done"});
        settings["hooks"]["Stop"] = json!([{"hooks": [theirs]}]);
        settings["hooks"]["StopFailure"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(theirs);
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m.install, &herdr());
        assert_eq!(
            out,
            format!(
                "failure Stop hook: not in {}; fix: loopherd install-hook\n\
                 doctor: 9 ok, 0 warnings, 1 failures\n",
                m.install.settings.display()
            )
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_hook_calling_a_relative_loopherd_fails() {
        let m = machine(&ALL);
        let mut settings: Value =
            serde_json::from_slice(&std::fs::read(&m.install.settings).unwrap()).unwrap();
        settings["hooks"]["Stop"][0]["hooks"][0]["command"] = json!("'loopherd' stop-hook || true");
        std::fs::write(&m.install.settings, settings.to_string()).unwrap();
        let (out, code) = run(&m.install, &herdr());
        assert!(
            out.starts_with("failure hook binary: hooks call loopherd, not the loopherd on PATH"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    const VERSION: &str = env!("CARGO_PKG_VERSION");

    fn plugin(enabled: bool, version: &str, linked: Option<&str>) -> Option<Plugin> {
        Some(Plugin {
            enabled,
            version: version.into(),
            linked: linked.map(PathBuf::from),
        })
    }

    fn with_plugin(plugin: Option<Plugin>, verbose: bool) -> (String, i32) {
        let m = machine(&ALL);
        let mut herdr = herdr();
        herdr.loopherd_plugin = plugin;
        let checks = checks(&m.install, &herdr);
        (text(&checks, verbose), exit_code(&checks))
    }

    #[test]
    fn a_missing_plugin_warns_naming_plugin_install_and_exits_0() {
        assert_eq!(
            with_plugin(None, false),
            (
                "warning herdr plugin: loopherd's herdr plugin isn't installed, so its \
                 actions, popups and event hooks are unavailable; \
                 fix: herdr plugin install junderdo/loopherd/herdr-plugin\n\
                 doctor: 9 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn a_disabled_plugin_warns_naming_plugin_enable() {
        assert_eq!(
            with_plugin(plugin(false, VERSION, None), false),
            (
                "warning herdr plugin: loopherd's herdr plugin is disabled, so its \
                 actions, popups and event hooks are unavailable; \
                 fix: herdr plugin enable loopherd\n\
                 doctor: 9 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn a_plugin_at_another_version_than_loopherd_warns() {
        let (out, code) = with_plugin(plugin(true, "0.0.1", None), false);
        assert_eq!(
            out,
            format!(
                "warning herdr plugin: plugin 0.0.1 differs from loopherd {VERSION}; \
                 fix: herdr plugin update loopherd\n\
                 doctor: 9 ok, 1 warnings, 0 failures\n"
            )
        );
        assert_eq!(code, 0);
        let (out, _) = with_plugin(plugin(true, "0.0.1", Some("/src/herdr-plugin")), false);
        assert!(
            out.starts_with(&format!(
                "warning herdr plugin: plugin 0.0.1 differs from loopherd {VERSION}; \
                 fix: bring the linked plugin at /src/herdr-plugin to loopherd {VERSION}\n"
            )),
            "{out}"
        );
    }

    #[test]
    fn a_plugin_newer_than_loopherd_names_updating_loopherd() {
        for linked in [None, Some("/src/herdr-plugin")] {
            let (out, code) = with_plugin(plugin(true, "999.0.0", linked), false);
            assert_eq!(
                out,
                format!(
                    "warning herdr plugin: plugin 999.0.0 differs from loopherd {VERSION}; \
                     fix: update loopherd to 999.0.0\n\
                     doctor: 9 ok, 1 warnings, 0 failures\n"
                )
            );
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn a_sound_plugin_is_listed_only_with_verbose() {
        let (out, _) = with_plugin(plugin(true, VERSION, None), true);
        assert!(
            out.contains(&format!("ok herdr plugin: {VERSION}, enabled\n")),
            "{out}"
        );
        let (out, _) = with_plugin(plugin(true, VERSION, Some("/src/herdr-plugin")), true);
        assert!(
            out.contains(&format!(
                "ok herdr plugin: {VERSION}, enabled, linked from /src/herdr-plugin\n"
            )),
            "{out}"
        );
    }

    fn herdr_failure(server: Server) -> String {
        let m = machine(&ALL);
        let mut herdr = herdr();
        herdr.server = Some(server);
        let (out, code) = run(&m.install, &herdr);
        assert_eq!(code, 1, "{out}");
        out.lines().next().unwrap().to_string()
    }

    #[test]
    fn a_herdr_that_does_not_answer_in_time_fails_naming_the_deadline() {
        let socket = Some(PathBuf::from("/run/wedged.sock"));
        assert_eq!(
            herdr_failure(Server::NoAnswer { socket }),
            "failure herdr: herdr didn't answer on /run/wedged.sock within 5s; \
             fix: restart herdr's server"
        );
        assert_eq!(
            herdr_failure(Server::NoAnswer { socket: None }),
            "failure herdr: herdr didn't answer on herdr's default socket within 5s; \
             fix: restart herdr's server"
        );
    }

    #[test]
    fn a_missing_herdr_or_one_without_a_version_fails_saying_which() {
        assert_eq!(
            herdr_failure(Server::Missing),
            "failure herdr: no herdr on PATH; fix: install herdr"
        );
        let socket = Some(PathBuf::from("/run/s.sock"));
        assert_eq!(
            herdr_failure(Server::Running {
                socket,
                version: None
            }),
            "failure herdr version: herdr's server didn't report a version; \
             fix: upgrade herdr to 0.9.5 or later and restart its server"
        );
    }

    struct Herds {
        dir: tempfile::TempDir,
    }

    impl Herds {
        /// Herd files whose `[queue]` holds `queue`'s lines, each herd's repo a temp dir.
        fn with(herds: &[(&str, &str)]) -> Herds {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("herds")).unwrap();
            std::fs::create_dir_all(dir.path().join("repo")).unwrap();
            for (name, queue) in herds {
                let text = format!(
                    "repo = {:?}\n[template.t]\nparams = [\"n\"]\nprompt = \"go\"\n{queue}\n",
                    dir.path().join("repo")
                );
                std::fs::write(dir.path().join(format!("herds/{name}.toml")), text).unwrap();
            }
            Herds { dir }
        }

        fn paths(&self) -> crate::herd::Paths {
            crate::herd::Paths {
                herds: self.dir.path().join("herds"),
                home: self.dir.path().to_path_buf(),
                feed_lock: self.dir.path().join("feed.lock"),
                paused: self.dir.path().join("paused"),
            }
        }

        fn run(&self, timeout: Duration) -> (String, i32) {
            let checks = tracker_checks(&self.paths(), timeout);
            (text(&checks, true), exit_code(&checks))
        }
    }

    fn queue(source: &str, backlog: Option<&str>) -> String {
        let backlog = backlog.map_or(String::new(), |b| format!("backlog = {b:?}\n"));
        format!("[queue]\ntemplate = \"t\"\nsource = {source:?}\n{backlog}max = 1")
    }

    #[test]
    fn trackers_report_each_exit_and_task_count_but_never_a_task() {
        let herds = Herds::with(&[
            (
                "a",
                &queue("printf 'LH-1\\n\\nLH-2\\n# note\\n'", Some("echo LH-3")),
            ),
            ("b", &queue("true", None)),
        ]);
        assert_eq!(
            herds.run(TRACKER_TIMEOUT),
            (
                "ok a source: exit 0, 2 tasks\n\
                 ok a backlog: exit 0, 1 tasks\n\
                 ok b source: exit 0, 0 tasks\n\
                 doctor: 3 ok, 0 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn trackers_run_in_the_herds_repo() {
        let herds = Herds::with(&[("a", &queue("test -d ../herds && ls", None))]);
        std::fs::write(herds.dir.path().join("repo/task"), "").unwrap();
        assert_eq!(
            herds.run(TRACKER_TIMEOUT).0,
            "ok a source: exit 0, 1 tasks\ndoctor: 1 ok, 0 warnings, 0 failures\n"
        );
    }

    #[test]
    fn a_failing_tracker_fails_naming_the_herd_and_command() {
        let herds = Herds::with(&[(
            "a",
            &queue("echo LH-1; echo nope >&2; exit 3", Some("exit 4")),
        )]);
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert_eq!(
            out,
            "failure a source: \"echo LH-1; echo nope >&2; exit 3\" failed (exit 3): nope; \
             fix: fix a's queue.source, e.g. loopherd herd set a queue.source <command>\n\
             failure a backlog: \"exit 4\" failed (exit 4); \
             fix: fix a's queue.backlog, e.g. loopherd herd set a queue.backlog <command>\n\
             doctor: 0 ok, 0 warnings, 2 failures\n"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_tracker_outliving_its_timeout_fails() {
        let herds = Herds::with(&[("a", &queue("sleep 5", None))]);
        let (out, code) = herds.run(Duration::from_millis(200));
        assert!(
            out.starts_with("failure a source: \"sleep 5\" timed out after 200ms; "),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_paused_herd_runs_and_a_herd_without_a_queue_has_nothing_to_run() {
        let herds = Herds::with(&[("a", &queue("echo x", None)), ("b", "")]);
        let paths = herds.paths();
        std::fs::create_dir_all(&paths.paused).unwrap();
        std::fs::write(
            paths.paused.join("a"),
            r#"{"since":"2026-01-01T00:00:00Z","reason":null}"#,
        )
        .unwrap();
        assert!(matches!(
            crate::pause::read(&paths.paused, "a"),
            crate::pause::Reading::Paused(_)
        ));
        assert_eq!(
            herds.run(TRACKER_TIMEOUT).0,
            "ok a source: exit 0, 1 tasks\ndoctor: 1 ok, 0 warnings, 0 failures\n"
        );
    }

    #[test]
    fn a_herd_file_that_does_not_load_fails_its_trackers() {
        let herds = Herds::with(&[("a", &queue("echo x", None))]);
        std::fs::write(herds.dir.path().join("herds/a.toml"), "repo = ").unwrap();
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert!(out.starts_with("failure a trackers: herd: "), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_failing_trackers_stderr_shows_only_its_last_line_printable_and_cut() {
        let long = "x".repeat(300);
        let source = format!("printf 'first\\n\\033[31mred\\033[0m {long}\\n' >&2; exit 1");
        let herds = Herds::with(&[("a", &queue(&source, None))]);
        let (out, _) = herds.run(TRACKER_TIMEOUT);
        let (_, stderr) = out.split_once("failed (exit 1): ").unwrap();
        let (stderr, _) = stderr.split_once("; fix: ").unwrap();
        assert_eq!(stderr, format!("red {}…", "x".repeat(196)));
    }

    #[test]
    fn a_missing_repo_fails_once_naming_herd_set_repo_and_runs_nothing() {
        let herds = Herds::with(&[("a", &queue("echo x", Some("echo y")))]);
        std::fs::remove_dir(herds.dir.path().join("repo")).unwrap();
        let (out, code) = herds.run(TRACKER_TIMEOUT);
        assert_eq!(
            out,
            format!(
                "failure a repo: {} isn't a directory, so its trackers can't run; \
                 fix: loopherd herd set a repo <path>\n\
                 doctor: 0 ok, 0 warnings, 1 failures\n",
                herds.dir.path().join("repo").display()
            )
        );
        assert_eq!(code, 1);
    }
}
