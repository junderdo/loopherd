//! Members' stop records, written by a Claude Stop hook loopherd installs, so
//! `watch` can tell a member paused on background work from a finished one.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr::{
    Agent, AgentStatus, Herdr, NewTab, Pane, StartOutcome, Tab, WaitOutcome, Workspace,
};
use crate::ledger::{Ledger, temp_path};
use crate::report::{Reporter, reporter};

/// A member's latest stop. A `None` array is unknown, not empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub at: String,
    pub background_tasks: Option<Vec<Value>>,
    pub session_crons: Option<Vec<Value>>,
}

impl Stop {
    /// What it left in flight, e.g. `1 shell, 1 cron`; `None` when nothing
    /// is known to be.
    pub fn paused(&self) -> Option<String> {
        let mut counts: Vec<(&str, usize)> = vec![];
        for task in self.background_tasks.iter().flatten() {
            let kind = task.get("type").and_then(Value::as_str).unwrap_or("task");
            match counts.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((kind, 1)),
            }
        }
        let crons = self.session_crons.as_ref().map_or(0, Vec::len);
        if crons > 0 {
            counts.push(("cron", crons));
        }
        let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Records the Stop hook `input` as the latest stop of the member `who`
/// resolves to; returns its name.
pub fn record(
    ledger: &dyn Ledger,
    dir: &Path,
    who: &Reporter,
    input: &[u8],
    now: SystemTime,
) -> Result<String> {
    let name = reporter(ledger, who)?;
    let input: Value = serde_json::from_slice(input).context("parse the Stop hook input")?;
    let array = |key: &str| input.get(key).and_then(Value::as_array).cloned();
    let stop = Stop {
        at: humantime::format_rfc3339_nanos(now).to_string(),
        background_tasks: array("background_tasks"),
        session_crons: array("session_crons"),
    };
    replace(&path(dir, &name), &serde_json::to_vec(&stop)?)?;
    Ok(name)
}

/// Writes `bytes` aside and renames them in, so a reader never sees half.
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = temp_path(path);
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

/// The member's latest stop; `None` when it has none yet.
pub fn latest(dir: &Path, name: &str) -> Result<Option<Stop>> {
    let path = path(dir, name);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The Stop hook loopherd adds to every member it starts.
pub struct StopHook {
    /// Where each member's merged settings file goes.
    pub settings: PathBuf,
    /// The shell command the hook runs.
    pub command: String,
}

impl StopHook {
    /// `args` with their `--settings` replaced by one file that adds the hook
    /// to it. Merged rather than passed alongside, since claude doesn't
    /// document how two `--settings` flags combine.
    fn add_to(&self, name: &str, args: &[String]) -> Result<Vec<String>> {
        let mut theirs = None;
        let mut rest = vec![];
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.strip_prefix("--settings") {
                Some("") => {
                    theirs = Some(args.next().context("--settings needs a value")?.as_str())
                }
                Some(value) if value.starts_with('=') => theirs = Some(&value[1..]),
                _ => rest.push(arg.clone()),
            }
        }
        let mut settings = match theirs {
            Some(value) => user_settings(value)?,
            None => serde_json::json!({}),
        };
        let stop = serde_json::json!({"hooks": [{"type": "command", "command": self.command}]});
        settings
            .as_object_mut()
            .context("--settings is not a JSON object")?
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("--settings hooks is not a JSON object")?
            .entry("Stop")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .context("--settings hooks.Stop is not an array")?
            .push(stop);
        let path = self.settings.join(format!("{name}.json"));
        replace(&path, &serde_json::to_vec_pretty(&settings)?)?;
        rest.extend(["--settings".into(), path.to_string_lossy().into_owned()]);
        Ok(rest)
    }
}

/// The user's `--settings` value: inline JSON or an absolute file path.
fn user_settings(value: &str) -> Result<Value> {
    if value.trim_start().starts_with('{') {
        return serde_json::from_str(value).context("parse --settings JSON");
    }
    let path = Path::new(value);
    // claude resolves a relative path from the member's cwd, which isn't ours.
    if !path.is_absolute() {
        bail!("--settings {value}: give an absolute path so loopherd can add its Stop hook");
    }
    let bytes = fs::read(path).with_context(|| format!("read --settings {value}"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse --settings {value}"))
}

/// A herdr that starts every Claude with `hook` added to its settings.
pub struct Hooked<'a> {
    pub inner: &'a dyn Herdr,
    pub hook: StopHook,
}

impl Herdr for Hooked<'_> {
    fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.inner.workspaces()
    }
    fn agents(&self) -> Result<Vec<Agent>> {
        self.inner.agents()
    }
    fn agent(&self, name: &str) -> Result<Agent> {
        self.inner.agent(name)
    }
    fn tabs_in(&self, workspace_id: &str) -> Result<Vec<Tab>> {
        self.inner.tabs_in(workspace_id)
    }
    fn panes_in(&self, workspace_id: &str) -> Result<Vec<Pane>> {
        self.inner.panes_in(workspace_id)
    }
    fn at_shell_prompt(&self, pane_id: &str) -> Result<bool> {
        self.inner.at_shell_prompt(pane_id)
    }
    fn create_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<NewTab> {
        self.inner.create_tab(workspace_id, cwd, label)
    }
    fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.inner.close_tab(tab_id)
    }
    fn start_claude(
        &self,
        name: &str,
        pane_id: &str,
        timeout: Duration,
        args: &[String],
    ) -> Result<StartOutcome> {
        let args = self.hook.add_to(name, args)?;
        self.inner.start_claude(name, pane_id, timeout, &args)
    }
    fn wait(&self, name: &str, until: &[AgentStatus], timeout: Duration) -> Result<WaitOutcome> {
        self.inner.wait(name, until, timeout)
    }
    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        self.inner.prompt(name, text)
    }
    fn focus(&self, pane_id: &str) -> Result<()> {
        self.inner.focus(pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::spawn::{SpawnRequest, spawn};

    pub(crate) fn member(name: &str, pane: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
            params: None,
        }
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    fn pane(pane: &str) -> Reporter<'_> {
        Reporter {
            own_pane: Some(pane),
            as_name: None,
        }
    }

    #[test]
    fn the_hook_records_the_latest_stop_of_the_member_in_its_pane() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger(
            [member("lh-1", "w1:p1"), member("lh-2", "w1:p2")]
                .into_iter()
                .map(|m| (m.name.clone(), m))
                .collect(),
        );
        let paused = br#"{"session_id":"s","hook_event_name":"Stop",
            "background_tasks":[{"id":"b1","type":"shell","status":"running","command":"sleep 240"}],
            "session_crons":[]}"#;
        let finished = br#"{"background_tasks":[],"session_crons":[]}"#;

        let name = record(
            &ledger,
            dir.path(),
            &pane("w1:p2"),
            paused,
            at("2026-11-02T08:05:40Z"),
        );
        assert_eq!(name.unwrap(), "lh-2");
        record(
            &ledger,
            dir.path(),
            &pane("w1:p2"),
            finished,
            at("2026-11-02T08:09:52Z"),
        )
        .unwrap();

        assert_eq!(
            latest(dir.path(), "lh-2").unwrap(),
            Some(Stop {
                at: "2026-11-02T08:09:52.000000000Z".into(),
                background_tasks: Some(vec![]),
                session_crons: Some(vec![]),
            })
        );
        assert_eq!(latest(dir.path(), "lh-1").unwrap(), None);
    }

    #[test]
    fn a_missing_array_is_recorded_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger([("lh-1".into(), member("lh-1", "w1:p1"))].into());

        record(
            &ledger,
            dir.path(),
            &pane("w1:p1"),
            br#"{"session_crons":[]}"#,
            at("2026-11-02T08:05:40Z"),
        )
        .unwrap();

        let stop = latest(dir.path(), "lh-1").unwrap().unwrap();
        assert_eq!(stop.background_tasks, None);
        assert_eq!(stop.session_crons, Some(vec![]));
    }

    #[test]
    fn a_pane_that_is_no_member_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = MemLedger([("lh-1".into(), member("lh-1", "w1:p1"))].into());

        let err = record(
            &ledger,
            dir.path(),
            &pane("w1:p9"),
            b"{}",
            SystemTime::now(),
        );

        assert!(err.unwrap_err().to_string().contains("not a member"));
        assert_eq!(latest(dir.path(), "lh-1").unwrap(), None);
    }

    const HOOK: &str = "'/bin/loopherd' stop-hook --ledger '/state/ledger.json'";

    fn hooked<'a>(herdr: &'a FakeHerdr, settings: &Path) -> Hooked<'a> {
        Hooked {
            inner: herdr,
            hook: StopHook {
                settings: settings.into(),
                command: HOOK.into(),
            },
        }
    }

    fn spawned_with(claude_args: &[&str]) -> (Vec<String>, Value, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let req = SpawnRequest {
            repo: "/src/repo".into(),
            name: "lh-1".into(),
            worktree: None,
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            claude_args: claude_args.iter().map(|a| a.to_string()).collect(),
            member_of: None,
        };
        let mut ledger = MemLedger::default();
        spawn(&hooked(&herdr, dir.path()), &mut ledger, &req, Some("w1")).unwrap();
        assert_eq!(
            ledger.0["lh-1"].claude_args, req.claude_args,
            "ledger keeps the user's args"
        );
        let args = herdr.started().remove(0).args;
        let at = args
            .iter()
            .rposition(|a| a == "--settings")
            .expect("--settings");
        let settings = fs::read(&args[at + 1]).unwrap();
        (args, serde_json::from_slice(&settings).unwrap(), dir)
    }

    fn stop_hook() -> Value {
        serde_json::json!({"hooks": [{"type": "command", "command": HOOK}]})
    }

    #[test]
    fn spawn_starts_a_member_with_a_settings_file_adding_the_stop_hook() {
        let (args, settings, dir) = spawned_with(&["--model", "opus"]);

        let file = dir.path().join("lh-1.json");
        assert_eq!(
            args,
            ["--model", "opus", "--settings", file.to_str().unwrap()]
        );
        assert_eq!(
            settings,
            serde_json::json!({"hooks": {"Stop": [stop_hook()]}})
        );
    }

    #[test]
    fn a_users_settings_file_is_merged_and_passed_once() {
        let user = tempfile::NamedTempFile::new().unwrap();
        let theirs = serde_json::json!({"type": "command", "command": "notify"});
        let user_settings = serde_json::json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [theirs]}],
                "SessionStart": [{"hooks": [theirs]}],
            },
        });
        fs::write(user.path(), user_settings.to_string()).unwrap();

        let (args, settings, _dir) =
            spawned_with(&["--settings", user.path().to_str().unwrap(), "--verbose"]);

        assert_eq!(
            args.iter().filter(|a| a.starts_with("--settings")).count(),
            1
        );
        assert_eq!(args[0], "--verbose");
        assert_eq!(
            settings,
            serde_json::json!({
                "model": "opus",
                "hooks": {
                    "Stop": [{"hooks": [theirs]}, stop_hook()],
                    "SessionStart": [{"hooks": [theirs]}],
                },
            })
        );
    }

    #[test]
    fn inline_json_settings_are_merged_too() {
        let (args, settings, _dir) = spawned_with(&[r#"--settings={"model": "opus"}"#]);

        assert_eq!(args.len(), 2);
        assert_eq!(
            settings,
            serde_json::json!({"model": "opus", "hooks": {"Stop": [stop_hook()]}})
        );
    }
}
