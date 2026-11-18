//! `loopherd doctor`: whether this machine's loopherd install, herdr, config,
//! herd files and state are sound. It only reads, and names the command that
//! fixes each problem.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use anyhow::{Result, bail};

use crate::herd::Paths;
use crate::herdr::{Herdr, Server};
use crate::ledger::{FileLedger, Ledger};
use crate::pause::Reading;

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

/// Where the config, herd and state checks read, gathered by the caller.
pub struct Setup {
    pub paths: Paths,
    pub ledger: FileLedger,
}

/// Every check, the herd, repo and state ones only for `herd` when given.
/// `setup` is `Err` when config and state can't be found; then they warn.
/// Fails when `herd` has no herd file.
pub fn checks(
    install: &Install,
    setup: &Result<Setup>,
    herd: Option<&str>,
    herdr: &dyn Herdr,
) -> Result<Vec<Check>> {
    if let (Some(herd), Ok(setup)) = (herd, setup) {
        crate::herd::require(&setup.paths, herd)?;
    }
    let mut checks = hook_checks(install);
    checks.push(running_binary(install));
    let (herdr_checks, reached) = herdr_checks(herdr);
    checks.extend(herdr_checks);
    match setup {
        Ok(setup) => {
            checks.extend(config_checks(&setup.paths));
            checks.extend(herd_checks(&setup.paths, herd));
            checks.extend(state_checks(
                setup,
                herd,
                reached.map(|socket| (herdr, socket)),
            ));
        }
        Err(e) if herd.is_some() => bail!("can't find herd files: {e:#}"),
        Err(e) => checks.push(Check {
            name: "config and state".into(),
            status: Status::Warning,
            detail: format!("not checked: {e:#}"),
            fix: Some("set HOME".into()),
        }),
    }
    Ok(checks)
}

fn config_checks(paths: &Paths) -> Vec<Check> {
    let config = crate::herd::config_file(paths);
    let by_hand = format!("fix {} by hand", config.display());
    let mut checks = vec![];
    let file = match crate::config::read(&config) {
        Ok(file) => {
            checks.push(ok("config", config.display().to_string()));
            Some(file)
        }
        Err(e) => {
            checks.push(failure("config", one_line(&e), by_hand.clone()));
            None
        }
    };
    if crate::herd::parses_as_toml(&config) {
        match crate::notify::load(&config) {
            Err(e) => checks.push(failure("notify", one_line(&e), by_hand)),
            Ok(Some(notify)) if notify.sends_nothing() => checks.push(Check {
                name: "notify".into(),
                status: Status::Warning,
                detail: format!(
                    "{}: [notify] is enabled with herdr = false, no command and no pane_label, \
                     so nothing is sent",
                    config.display()
                ),
                fix: Some(format!(
                    "turn on one of them, or [notify] enabled = false, {by_hand}"
                )),
            }),
            Ok(_) => {}
        }
    }
    let Some(file) = file else {
        return checks;
    };
    let unset = |name: &str, is_unset: bool| match is_unset {
        true => Check {
            name: name.into(),
            status: Status::Warning,
            detail: format!("unset in {}", config.display()),
            fix: Some("loopherd setup".into()),
        },
        false => ok(name, format!("set in {}", config.display())),
    };
    let shepherd = file.shepherd.as_ref().and_then(|s| s.context_limit);
    let limits_unset =
        file.context_window.is_none() && file.default_context_limit.is_none() && shepherd.is_none();
    checks.push(unset("lead defaults", file.lead_defaults.is_none()));
    checks.push(unset("context limits", limits_unset));
    checks
}

fn herd_checks(paths: &Paths, only: Option<&str>) -> Vec<Check> {
    let herds = match only {
        Some(herd) => vec![herd.to_string()],
        None => match crate::herd::names(&paths.herds) {
            Ok(names) => names,
            Err(e) => {
                let fix = format!("fix {} by hand", paths.herds.display());
                return vec![failure("herd files", one_line(&e), fix)];
            }
        },
    };
    let mut checks = vec![];
    for herd in herds {
        let file = paths.herds.join(format!("{herd}.toml"));
        let name = format!("herd {herd}");
        let repo = match crate::herd::checked_repo(paths, &herd) {
            Ok(repo) => repo,
            Err(invalid) => {
                let fix = format!("fix {} by hand", file.display());
                checks.push(failure(&name, invalid, fix));
                continue;
            }
        };
        checks.push(ok(&name, file.display().to_string()));
        let name = format!("herd {herd} repo");
        let set_repo = format!("loopherd herd set {herd} repo '\"<path>\"'");
        checks.push(match repo.exists() {
            false => failure(&name, format!("{} is missing", repo.display()), set_repo),
            true if !repo.is_dir() => failure(
                &name,
                format!("{} is not a directory", repo.display()),
                set_repo,
            ),
            // A worktree's `.git` is a file.
            true if !repo.join(".git").exists() => failure(
                &name,
                format!("{} is not a git repo", repo.display()),
                set_repo,
            ),
            true => ok(&name, repo.display().to_string()),
        });
    }
    checks
}

/// The ledger, pause files and mailboxes, and, when herdr was reached, each
/// unretired member's pane on its socket.
fn state_checks(
    setup: &Setup,
    only: Option<&str>,
    reached: Option<(&dyn Herdr, Option<PathBuf>)>,
) -> Vec<Check> {
    let mut checks = pause_checks(&setup.paths, only);
    let entries = match setup.ledger.entries() {
        Ok(entries) => entries,
        Err(e) => {
            checks.insert(
                0,
                failure("ledger", one_line(&e), "fix the ledger by hand".into()),
            );
            return checks;
        }
    };
    checks.insert(0, ok("ledger", format!("{} entries", entries.len())));
    let members: Vec<_> = entries
        .into_values()
        .filter(|r| only.is_none_or(|herd| r.herd.as_deref() == Some(herd)))
        .collect();
    if let Ok(dir) = setup.ledger.reports_dir() {
        for member in &members {
            checks.extend(mailbox_check(&dir, &member.name));
        }
    }
    if let Some((herdr, socket)) = reached {
        let unretired: Vec<_> = members.iter().filter(|r| !r.retired).collect();
        checks.extend(pane_checks(herdr, socket.as_deref(), &unretired));
    }
    checks
}

fn pause_checks(paths: &Paths, only: Option<&str>) -> Vec<Check> {
    let dir = &paths.paused;
    let readings = match only {
        Some(herd) => vec![(herd.to_string(), crate::pause::read(dir, herd))],
        None => match crate::pause::all(dir) {
            Ok(readings) => readings,
            Err(e) => {
                let fix = format!("fix {} by hand", dir.display());
                return vec![failure("pause files", one_line(&e), fix)];
            }
        },
    };
    readings
        .into_iter()
        .filter_map(|(herd, reading)| match reading {
            Reading::Unreadable(e) => {
                // `pause` needs the herd file; `resume` deletes an orphaned pause too.
                let fix = match crate::herd::require(paths, &herd) {
                    Ok(()) => {
                        format!("loopherd resume {herd}, or loopherd pause {herd} to rewrite it")
                    }
                    Err(_) => format!("loopherd resume {herd}"),
                };
                Some(failure(&format!("pause {herd}"), one_line(&e), fix))
            }
            _ => None,
        })
        .collect()
}

/// A failure when `name`'s mailbox can't be read or has a line that doesn't parse.
fn mailbox_check(dir: &Path, name: &str) -> Option<Check> {
    let detail = match crate::report::history(dir, name) {
        Err(e) => one_line(&e),
        Ok((_, warnings)) => match warnings.as_slice() {
            [] => return None,
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} (and {} more bad lines)", rest.len()),
        },
    };
    let fix = "fix the mailbox's bad lines by hand".into();
    Some(failure(&format!("mailbox {name}"), detail, fix))
}

/// Each member on `here`'s herdr whose pane is gone warns; those on another
/// socket are counted, not checked.
fn pane_checks(
    herdr: &dyn Herdr,
    here: Option<&Path>,
    members: &[&crate::ledger::AgentRecord],
) -> Vec<Check> {
    if members.is_empty() {
        return vec![];
    }
    let agents = match herdr.agents() {
        Ok(agents) => agents,
        Err(e) => {
            return vec![failure(
                "members",
                one_line(&e),
                "restart herdr's server".into(),
            )];
        }
    };
    let on = here.map_or("herdr's default socket".into(), |s| s.display().to_string());
    // With herdr's socket unknown, a recorded one can't be shown to be this herdr's.
    let elsewhere = |r: &&crate::ledger::AgentRecord| match (here, &r.socket) {
        (Some(here), Some(there)) => !same_file(here, there),
        (None, Some(_)) => true,
        (_, None) => false,
    };
    let (other, local): (Vec<_>, Vec<_>) = members.iter().copied().partition(elsewhere);
    let mut checks: Vec<Check> = local
        .iter()
        .filter(|r| crate::matching::live_agent(r, &agents).is_none())
        .map(|r| Check {
            name: format!("member {}", r.name),
            status: Status::Warning,
            detail: format!("no live agent matches it (pane {}) on {on}", r.pane_id),
            fix: Some(format!(
                "loopherd revive --name {0}, or loopherd retire {0}",
                r.name
            )),
        })
        .collect();
    let live = local.len() - checks.len();
    if live > 0 {
        checks.push(ok("members", format!("{live} live on {on}")));
    }
    if !other.is_empty() {
        checks.push(ok(
            "members elsewhere",
            format!("{} on another herdr socket, not checked", other.len()),
        ));
    }
    checks
}

fn one_line(e: &anyhow::Error) -> String {
    format!("{e:#}").trim_end().to_string()
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

/// How long herdr's status probe may take; a wedged server never answers.
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

/// The herdr checks, and the socket of the herdr they reached, if any.
fn herdr_checks(herdr: &dyn Herdr) -> (Vec<Check>, Option<Option<PathBuf>>) {
    let (checks, reached) = match herdr.server(HERDR_TIMEOUT) {
        Ok(Server::Running { socket, version }) => {
            (running_checks(socket.clone(), version), Some(socket))
        }
        not_running => (vec![unreached(not_running)], None),
    };
    (checks, reached)
}

fn on(socket: Option<PathBuf>) -> String {
    match socket {
        Some(socket) => socket.display().to_string(),
        None => "herdr's default socket".into(),
    }
}

fn unreached(server: Result<Server>) -> Check {
    match server {
        Ok(Server::Running { .. }) => unreachable!("a running server is reached"),
        Ok(Server::NotRunning { socket }) => {
            let detail = format!("no herdr server answers on {}", on(socket));
            failure("herdr", detail, "herdr".into())
        }
        Ok(Server::NoAnswer { socket }) => {
            let detail = format!(
                "herdr didn't answer on {} within {}",
                on(socket),
                humantime::format_duration(HERDR_TIMEOUT)
            );
            failure("herdr", detail, "restart herdr's server".into())
        }
        Ok(Server::Missing) => failure("herdr", "no herdr on PATH".into(), "install herdr".into()),
        Err(e) => failure("herdr", one_line(&e), "restart herdr's server".into()),
    }
}

fn running_checks(socket: Option<PathBuf>, version: Option<String>) -> Vec<Check> {
    let socket = on(socket);
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
    use crate::ledger::AgentRecord;
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
        setup: Result<Setup>,
    }

    impl Machine {
        fn paths(&self) -> &Paths {
            &self.setup.as_ref().unwrap().paths
        }

        fn config(&self) -> PathBuf {
            crate::herd::config_file(self.paths())
        }

        fn herd_file(&self, herd: &str, text: &str) {
            std::fs::create_dir_all(&self.paths().herds).unwrap();
            std::fs::write(self.paths().herds.join(format!("{herd}.toml")), text).unwrap();
        }

        /// A herd file for `herd` whose repo is a fresh git repo.
        fn herd(&self, herd: &str) -> PathBuf {
            let repo = self.dir.path().join(format!("repos/{herd}"));
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            self.herd_file(herd, &format!("repo = {:?}\n", repo.to_str().unwrap()));
            repo
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn ledger(&self, records: &[AgentRecord]) {
            let mut ledger = FileLedger::new(self.state().join("ledger.json"));
            for record in records {
                ledger.record(record.clone()).unwrap();
            }
        }

        fn checks(&self, herd: Option<&str>, herdr: &FakeHerdr) -> Result<Vec<Check>> {
            checks(&self.install, &self.setup, herd, herdr)
        }
    }

    const CONFIG: &str = "context_window = 200000\n[lead_defaults]\nmerge = false\n";

    /// A settings file whose hooks for `events` call `exe`, with `exe` both
    /// on PATH and running.
    fn machine(events: &[&str]) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/loopherd");
        let settings = dir.path().join("settings.json");
        write_hooks(&settings, &exe, events);
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/config.toml"), CONFIG).unwrap();
        let setup = Setup {
            paths: Paths {
                herds: dir.path().join("config/herds"),
                home: dir.path().to_path_buf(),
                feed_lock: dir.path().join("state/feed.lock"),
                paused: dir.path().join("state/paused"),
            },
            ledger: FileLedger::new(dir.path().join("state/ledger.json")),
        };
        Machine {
            install: Install {
                settings,
                settings_given: false,
                pane_label: false,
                on_path: Some(exe.clone()),
                running: exe,
            },
            setup: Ok(setup),
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

    fn run(m: &Machine, herdr: &FakeHerdr) -> (String, i32) {
        run_on(m, None, herdr)
    }

    fn run_on(m: &Machine, herd: Option<&str>, herdr: &FakeHerdr) -> (String, i32) {
        let checks = m.checks(herd, herdr).unwrap();
        (text(&checks, false), exit_code(&checks))
    }

    #[test]
    fn a_sound_install_prints_only_the_summary_and_exits_0() {
        let m = machine(&ALL);
        assert_eq!(
            run(&m, &herdr()),
            ("doctor: 13 ok, 0 warnings, 0 failures\n".into(), 0)
        );
    }

    #[test]
    fn a_missing_watched_or_tool_hook_fails_naming_install_hook() {
        for missing in ["Stop", "StopFailure", "SessionStart", "PreToolUse"] {
            let events: Vec<&str> = ALL.into_iter().filter(|e| *e != missing).collect();
            let m = machine(&events);
            let (out, code) = run(&m, &herdr());
            assert_eq!(
                out,
                format!(
                    "failure {missing} hook: not in {}; fix: loopherd install-hook\n\
                     doctor: 12 ok, 0 warnings, 1 failures\n",
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
            run(&m, &herdr()),
            (
                format!("warning {line}doctor: 12 ok, 1 warnings, 0 failures\n"),
                0
            )
        );
        m.install.pane_label = true;
        assert_eq!(
            run(&m, &herdr()),
            (
                format!("failure {line}doctor: 12 ok, 0 warnings, 1 failures\n"),
                1
            )
        );
    }

    #[test]
    fn a_settings_flag_is_repeated_in_the_fix() {
        let mut m = machine(&ALL[..3]);
        m.install.settings_given = true;
        let (out, _) = run(&m, &herdr());
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
        let (out, code) = run(&m, &herdr());
        assert!(
            out.ends_with("doctor: 8 ok, 1 warnings, 4 failures\n"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn unparseable_settings_fail_once_naming_the_file() {
        let m = machine(&ALL);
        std::fs::write(&m.install.settings, "{ not json").unwrap();
        let (out, code) = run(&m, &herdr());
        let first = out.lines().next().unwrap();
        assert!(
            first.starts_with(&format!(
                "failure settings: parse {}",
                m.install.settings.display()
            )),
            "{out}"
        );
        assert!(
            out.ends_with("doctor: 7 ok, 0 warnings, 1 failures\n"),
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
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "failure hook binary: hooks call {}, not the loopherd on PATH ({}); \
                 fix: loopherd install-hook\n\
                 doctor: 12 ok, 0 warnings, 1 failures\n",
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
        let (out, code) = run(&m, &herdr());
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
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "warning running binary: this is {}, not the loopherd on PATH ({}); \
                 fix: run the loopherd on PATH\n\
                 doctor: 12 ok, 1 warnings, 0 failures\n",
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
        assert_eq!(run(&m, &herdr()).1, 0);
        assert!(run(&m, &herdr()).0.starts_with("doctor: 13 ok"));
    }

    #[test]
    fn an_unreachable_herdr_fails_naming_its_socket() {
        let m = machine(&ALL);
        let herdr = herdr();
        herdr.set_unreachable(true);
        let (out, code) = run(&m, &herdr);
        assert_eq!(
            out,
            "failure herdr: no herdr server answers on /run/fake/herdr.sock; fix: herdr\n\
             doctor: 11 ok, 0 warnings, 1 failures\n"
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
            let (out, code) = run(&m, &herdr);
            match ok {
                true => assert_eq!(code, 0, "{version}: {out}"),
                false => assert_eq!(
                    out,
                    format!(
                        "failure herdr version: {version} is older than {MIN_HERDR}; \
                         fix: upgrade herdr to {MIN_HERDR} or later and restart its server\n\
                         doctor: 12 ok, 0 warnings, 1 failures\n"
                    )
                ),
            }
        }
    }

    #[test]
    fn verbose_lists_every_check() {
        let m = machine(&ALL);
        let checks = checks(&m.install, &m.setup, None, &herdr()).unwrap();
        let exe = m.install.running.display();
        let settings = m.install.settings.display();
        let config = m.config().display().to_string();
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
                 ok config: {config}\n\
                 ok lead defaults: set in {config}\n\
                 ok context limits: set in {config}\n\
                 ok ledger: 0 entries\n\
                 doctor: 13 ok, 0 warnings, 0 failures\n"
            )
        );
    }

    #[test]
    fn json_gives_every_check_and_the_counts() {
        let m = machine(&ALL[..4]);
        let checks = checks(&m.install, &m.setup, None, &herdr()).unwrap();
        let data: Value = serde_json::from_str(&json(&checks)).unwrap();
        assert_eq!(
            (&data["ok"], &data["warnings"], &data["failures"]),
            (&json!(12), &json!(1), &json!(0))
        );
        assert_eq!(data["checks"].as_array().unwrap().len(), 13);
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
        let (out, _) = run(&m, &herdr());
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
        let (out, code) = run(&m, &herdr());
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
        let (out, code) = run(&m, &herdr());
        assert_eq!(
            out,
            format!(
                "failure Stop hook: not in {}; fix: loopherd install-hook\n\
                 doctor: 12 ok, 0 warnings, 1 failures\n",
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
        let (out, code) = run(&m, &herdr());
        assert!(
            out.starts_with("failure hook binary: hooks call loopherd, not the loopherd on PATH"),
            "{out}"
        );
        assert_eq!(code, 1);
    }

    fn herdr_failure(server: Server) -> String {
        let m = machine(&ALL);
        let mut herdr = herdr();
        herdr.server = Some(server);
        let (out, code) = run(&m, &herdr);
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

    fn problems(m: &Machine, herdr: &FakeHerdr) -> Vec<String> {
        problems_on(m, None, herdr)
    }

    /// Each warning and failure line, without the summary.
    fn problems_on(m: &Machine, herd: Option<&str>, herdr: &FakeHerdr) -> Vec<String> {
        let (out, _) = run_on(m, herd, herdr);
        out.lines()
            .filter(|l| !l.starts_with("doctor: "))
            .map(str::to_string)
            .collect()
    }

    fn member(name: &str, herd: &str, pane: &str, socket: Option<&str>) -> AgentRecord {
        serde_json::from_value(json!({
            "name": name, "pane_id": pane, "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd, "socket": socket,
        }))
        .unwrap()
    }

    #[test]
    fn a_config_that_does_not_load_fails_naming_the_file_and_error() {
        let m = machine(&ALL);
        let config = m.config();
        for (text, error) in [
            ("context_window = [", "TOML parse error"),
            ("colour = 1\n", "unknown field `colour`"),
            (
                "context_window = 5\n",
                "context_window must be at least 10000, got 5",
            ),
        ] {
            std::fs::write(&config, text).unwrap();
            let (out, code) = run(&m, &herdr());
            assert!(
                out.starts_with(&format!("failure config: {}: ", config.display())),
                "{out}"
            );
            assert!(out.contains(error), "{out}");
            let fix = format!("; fix: fix {} by hand\ndoctor: ", config.display());
            assert!(out.contains(&fix), "{out}");
            assert_eq!(code, 1);
        }
    }

    #[test]
    fn a_notify_table_that_does_not_load_fails_naming_the_file() {
        let m = machine(&ALL);
        let config = m.config();
        std::fs::write(
            &config,
            format!("{CONFIG}[notify]\nenabled = true\nloud = 1\n"),
        )
        .unwrap();
        let (out, code) = run(&m, &herdr());
        let line = out.lines().next().unwrap();
        assert!(
            line.starts_with(&format!("failure notify: {}: [notify]: ", config.display())),
            "{out}"
        );
        assert!(line.contains("unknown field `loud`"), "{out}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_missing_config_is_fine_but_its_unset_settings_warn_naming_setup() {
        let m = machine(&ALL);
        std::fs::remove_file(m.config()).unwrap();
        let config = m.config().display().to_string();
        assert_eq!(
            run(&m, &herdr()),
            (
                format!(
                    "warning lead defaults: unset in {config}; fix: loopherd setup\n\
                     warning context limits: unset in {config}; fix: loopherd setup\n\
                     doctor: 11 ok, 2 warnings, 0 failures\n"
                ),
                0
            )
        );
    }

    #[test]
    fn any_one_limit_set_counts_as_set() {
        let m = machine(&ALL);
        for limit in [
            "default_context_limit = 100000",
            "shepherd.context_limit = 100000",
        ] {
            std::fs::write(
                m.config(),
                format!("{limit}\n[lead_defaults]\nmerge = true\n"),
            )
            .unwrap();
            assert_eq!(problems(&m, &herdr()), Vec::<String>::new());
        }
    }

    #[test]
    fn an_invalid_herd_file_fails_with_the_line_herds_gives() {
        let m = machine(&ALL);
        m.herd("good");
        m.herd_file("bad", "repo = \"/r\"\ncolour = 1\n");
        let (herds, _) = crate::herd::herds(m.paths(), &m.install.settings).unwrap();
        let file = m.paths().herds.join("bad.toml");
        let (out, code) = run(&m, &herdr());
        let fix = format!("; fix: fix {} by hand\n", file.display());
        let detail = out
            .strip_prefix("failure herd bad: ")
            .and_then(|rest| rest.split_once(&fix))
            .map(|(detail, _)| detail)
            .unwrap_or_else(|| panic!("{out}"));
        assert!(detail.starts_with("invalid bad.toml: "), "{out}");
        assert!(detail.contains("unknown field `colour`"), "{out}");
        assert!(herds.contains(&format!("{detail}\n")), "{herds}");
        assert_eq!(code, 1);
    }

    #[test]
    fn a_herd_file_is_checked_without_running_its_commands() {
        let m = machine(&ALL);
        let repo = m.herd("q");
        let ran = m.dir.path().join("ran");
        m.herd_file(
            "q",
            &format!(
                "repo = {:?}\n[template.w]\nparams = []\nprompt = \"go\"\n[queue]\nmax = 1\ntemplate = \"w\"\nsource = \"touch {}\"\nbacklog = \"touch {}\"\n",
                repo.to_str().unwrap(),
                ran.display(),
                ran.display()
            ),
        );
        let (_, code) = run(&m, &herdr());
        assert!(!ran.exists());
        assert_eq!(code, 0, "{:?}", problems(&m, &herdr()));
    }

    #[test]
    fn a_herd_repo_that_is_missing_or_not_a_git_repo_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::remove_dir(repo.join(".git")).unwrap();
        let fix = "fix: loopherd herd set h repo '\"<path>\"'";
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is not a git repo; {fix}",
                repo.display()
            )]
        );
        std::fs::remove_dir(&repo).unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is missing; {fix}",
                repo.display()
            )]
        );
    }

    #[test]
    fn a_repo_given_from_home_is_expanded() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::write(m.paths().herds.join("h.toml"), "repo = \"~/repos/h\"\n").unwrap();
        let checks = m.checks(None, &herdr()).unwrap();
        let check = checks.iter().find(|c| c.name == "herd h repo").unwrap();
        assert_eq!(
            (check.status, &check.detail),
            (Status::Ok, &repo.display().to_string())
        );
    }

    #[test]
    fn an_unparseable_ledger_fails_naming_it() {
        let m = machine(&ALL);
        std::fs::create_dir_all(m.state()).unwrap();
        std::fs::write(m.state().join("ledger.json"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        let ledger = m.state().join("ledger.json");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure ledger: parse ledger {}: ",
                ledger.display()
            )),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unparseable_pause_file_fails_naming_it() {
        let m = machine(&ALL);
        m.herd("h");
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("h"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "failure pause h: parse {}: ",
                paused.join("h").display()
            )),
            "{problems:?}"
        );
        assert!(
            problems[0].ends_with("fix: loopherd resume h, or loopherd pause h to rewrite it"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_mailbox_with_a_bad_line_fails_naming_it() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("a"), None, "w1:p1");
        m.ledger(&[member("a", "h", "w1:p1", None)]);
        let reports = m.state().join("reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("a.jsonl"), "nope\nnope either\n").unwrap();
        let problems = problems(&m, &herdr);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let mailbox = reports.join("a.jsonl");
        assert!(
            problems[0].starts_with(&format!(
                "failure mailbox a: skipped a bad line in {}: ",
                mailbox.display()
            )),
            "{problems:?}"
        );
        assert!(
            problems[0].contains("(and 1 more bad lines)"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_member_whose_pane_is_gone_warns_naming_revive_or_retire() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("live"), None, "w1:p1");
        let mut retired = member("old", "h", "w1:p3", None);
        retired.retired = true;
        m.ledger(&[
            member("live", "h", "w1:p1", None),
            member("gone", "h", "w1:p2", None),
            retired,
        ]);
        assert_eq!(
            run(&m, &herdr),
            (
                "warning member gone: no live agent matches it (pane w1:p2) on /run/fake/herdr.sock; \
                 fix: loopherd revive --name gone, or loopherd retire gone\n\
                 doctor: 14 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
    }

    #[test]
    fn members_on_another_socket_are_counted_in_one_line_not_checked() {
        let m = machine(&ALL);
        m.ledger(&[
            member("here", "h", "w1:p1", Some("/run/fake/herdr.sock")),
            member("dev1", "h", "w1:p2", Some("/run/dev.sock")),
            member("dev2", "h", "w1:p3", Some("/run/dev.sock")),
        ]);
        let checks = m.checks(None, &herdr()).unwrap();
        let lines = text(&checks, true);
        assert!(
            lines.contains("ok members elsewhere: 2 on another herdr socket, not checked\n"),
            "{lines}"
        );
        assert!(!lines.contains("member dev"), "{lines}");
        assert!(lines.contains("warning member here: "), "{lines}");
    }

    #[test]
    fn strays_are_not_reported() {
        let m = machine(&ALL);
        let herdr = herdr().with_live_agent(Some("stray"), None, "w1:p9");
        assert_eq!(run(&m, &herdr).0, "doctor: 13 ok, 0 warnings, 0 failures\n");
    }

    #[test]
    fn an_unreached_herdr_skips_the_pane_checks() {
        let m = machine(&ALL);
        m.ledger(&[member("gone", "h", "w1:p2", None)]);
        let herdr = herdr();
        herdr.set_unreachable(true);
        assert_eq!(
            problems(&m, &herdr),
            ["failure herdr: no herdr server answers on /run/fake/herdr.sock; fix: herdr"]
        );
    }

    #[test]
    fn a_herd_limits_herd_file_repo_and_state_checks_to_it() {
        let m = machine(&ALL);
        m.herd("h");
        m.herd_file("other", "colour = 1\n");
        m.ledger(&[
            member("a", "h", "w1:p1", None),
            member("b", "other", "w1:p2", None),
        ]);
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("other"), "{ nope").unwrap();
        std::fs::write(m.config(), "colour = 1\n").unwrap();
        let names = |herd| -> Vec<String> {
            problems_on(&m, Some(herd), &herdr())
                .iter()
                .filter(|p| p.starts_with("failure ") || p.starts_with("warning "))
                .map(|p| p.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(names("h"), ["failure config", "warning member a"]);
        std::fs::write(m.config(), CONFIG).unwrap();
        assert_eq!(
            names("other"),
            [
                "failure herd other",
                "failure pause other",
                "warning member b"
            ]
        );
    }

    #[test]
    fn an_unknown_herd_is_an_error() {
        let m = machine(&ALL);
        m.herd("h");
        let err = m.checks(Some("nope"), &herdr()).unwrap_err();
        assert!(format!("{err:#}").contains("nope.toml"), "{err:#}");
    }

    #[test]
    fn config_and_state_that_cannot_be_found_warn_and_the_rest_still_runs() {
        let mut m = machine(&ALL);
        m.setup = Err(anyhow::anyhow!("HOME is not set"));
        assert_eq!(
            run(&m, &herdr()),
            (
                "warning config and state: not checked: HOME is not set; \
                 fix: set HOME\n\
                 doctor: 9 ok, 1 warnings, 0 failures\n"
                    .into(),
                0
            )
        );
        assert!(m.checks(Some("h"), &herdr()).is_err());
    }

    #[test]
    fn an_unparseable_pause_of_a_herd_without_a_file_names_only_resume() {
        let m = machine(&ALL);
        let paused = &m.paths().paused;
        std::fs::create_dir_all(paused).unwrap();
        std::fs::write(paused.join("ghost"), "{ nope").unwrap();
        let problems = problems(&m, &herdr());
        assert!(
            problems[0].ends_with("; fix: loopherd resume ghost"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_herd_repo_that_is_a_file_fails() {
        let m = machine(&ALL);
        let repo = m.herd("h");
        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::write(&repo, "").unwrap();
        assert_eq!(
            problems(&m, &herdr()),
            [format!(
                "failure herd h repo: {} is not a directory; fix: loopherd herd set h repo '\"<path>\"'",
                repo.display()
            )]
        );
    }

    #[test]
    fn with_herdrs_socket_unknown_members_recorded_on_a_socket_are_counted_not_checked() {
        let m = machine(&ALL);
        m.ledger(&[member("dev", "h", "w1:p2", Some("/run/dev.sock"))]);
        let mut herdr = herdr();
        herdr.server = Some(Server::Running {
            socket: None,
            version: Some(MIN_HERDR.into()),
        });
        let checks = m.checks(None, &herdr).unwrap();
        let lines = text(&checks, true);
        assert!(
            lines.contains("ok members elsewhere: 1 on another herdr socket, not checked\n"),
            "{lines}"
        );
        assert!(!lines.contains("warning member"), "{lines}");
    }

    #[test]
    fn a_notify_table_that_sends_nothing_warns_as_herds_does() {
        let m = machine(&ALL);
        let config = m.config();
        std::fs::write(
            &config,
            format!("{CONFIG}[notify]\nenabled = true\nherdr = false\n"),
        )
        .unwrap();
        let problems = problems(&m, &herdr());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with(&format!(
                "warning notify: {}: [notify] is enabled with herdr = false, no command and no pane_label, so nothing is sent; ",
                config.display()
            )),
            "{problems:?}"
        );
    }
}
