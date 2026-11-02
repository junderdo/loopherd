//! The Claude Stop hook loopherd adds to every member it starts, through a
//! herdr wrapper that rewrites the member's `--settings`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::herdr::{
    Agent, AgentStatus, Herdr, NewTab, Pane, StartOutcome, Tab, WaitOutcome, Workspace,
};
use crate::shell::quote;
use crate::stop::replace;

/// Seconds Claude gives the hook before moving on.
const TIMEOUT_SECS: u64 = 10;

pub struct StopHook {
    /// Where each member's merged settings file goes.
    settings: PathBuf,
    /// The shell command the hook runs.
    command: String,
}

impl StopHook {
    /// The hook running `exe stop-hook` for members of the ledger at `ledger`.
    pub fn new(exe: &Path, ledger: &Path) -> Result<StopHook> {
        let state = ledger.parent().context("ledger path has no parent")?;
        // Exit 2 from a Stop hook blocks the stop, and a usage error (an old
        // binary swapped in, a bad flag) exits 2.
        let command = format!(
            "{} stop-hook --ledger {} || true",
            quote(&exe.to_string_lossy()),
            quote(&ledger.to_string_lossy()),
        );
        Ok(StopHook {
            settings: state.join("settings"),
            command,
        })
    }

    /// `args` with their `--settings` replaced by one file that adds the hook
    /// to it. Merged rather than passed alongside, since claude doesn't
    /// document how two `--settings` flags combine.
    fn add_to(&self, name: &str, cwd: &Path, args: &[String]) -> Result<Vec<String>> {
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
            Some(value) => user_settings(value, cwd)?,
            None => serde_json::json!({}),
        };
        let stop = serde_json::json!({"hooks": [{
            "type": "command",
            "command": self.command,
            "timeout": TIMEOUT_SECS,
        }]});
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

/// The user's `--settings` value: inline JSON or a file path, which claude
/// resolves from the member's cwd.
fn user_settings(value: &str, cwd: &Path) -> Result<Value> {
    if value.trim_start().starts_with('{') {
        return serde_json::from_str(value).context("parse --settings JSON");
    }
    let path = cwd.join(value);
    let bytes =
        std::fs::read(&path).with_context(|| format!("read --settings {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse --settings {}", path.display()))
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
        cwd: &Path,
        timeout: Duration,
        args: &[String],
    ) -> Result<StartOutcome> {
        let args = self.hook.add_to(name, cwd, args)?;
        self.inner.start_claude(name, pane_id, cwd, timeout, &args)
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;
    use crate::spawn::{SpawnRequest, spawn};

    /// A state dir and repo, both with spaces in their paths.
    struct World {
        _root: tempfile::TempDir,
        ledger: PathBuf,
        repo: PathBuf,
    }

    fn world() -> World {
        let root = tempfile::tempdir().unwrap();
        let ledger = root.path().join("state dir/ledger.json");
        let repo = root.path().join("my repo");
        fs::create_dir_all(&repo).unwrap();
        World {
            ledger,
            repo,
            _root: root,
        }
    }

    const EXE: &str = "/opt/my tools/loopherd";

    fn hooked<'a>(herdr: &'a FakeHerdr, world: &World) -> Hooked<'a> {
        Hooked {
            inner: herdr,
            hook: StopHook::new(Path::new(EXE), &world.ledger).unwrap(),
        }
    }

    fn request(world: &World, claude_args: &[&str]) -> SpawnRequest {
        SpawnRequest {
            repo: world.repo.clone(),
            name: "lh-1".into(),
            worktree: None,
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            claude_args: claude_args.iter().map(|a| a.to_string()).collect(),
            member_of: None,
        }
    }

    fn spawn_with(world: &World, claude_args: &[&str]) -> Result<(Vec<String>, Value)> {
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let req = request(world, claude_args);
        let mut ledger = MemLedger::default();
        spawn(&hooked(&herdr, world), &mut ledger, &req, Some("w1"))?;
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
        Ok((args, serde_json::from_slice(&settings).unwrap()))
    }

    fn command(world: &World) -> String {
        format!(
            "'{EXE}' stop-hook --ledger '{}' || true",
            world.ledger.display()
        )
    }

    fn stop_hook(world: &World) -> Value {
        serde_json::json!({"hooks": [{
            "type": "command",
            "command": command(world),
            "timeout": 10,
        }]})
    }

    #[test]
    fn spawn_starts_a_member_with_a_settings_file_adding_the_stop_hook() {
        let world = world();

        let (args, settings) = spawn_with(&world, &["--model", "opus"]).unwrap();

        let file = world.ledger.with_file_name("settings/lh-1.json");
        assert_eq!(
            args,
            ["--model", "opus", "--settings", file.to_str().unwrap()]
        );
        assert_eq!(
            settings,
            serde_json::json!({"hooks": {"Stop": [stop_hook(&world)]}})
        );
    }

    #[test]
    fn the_hook_command_exits_0_when_loopherd_fails_with_a_usage_error() {
        let world = world();
        let bin = world.repo.join("old bin");
        fs::create_dir_all(&bin).unwrap();
        // An older loopherd without `stop-hook`: clap exits 2 on usage errors.
        let old = bin.join("loopherd");
        fs::write(
            &old,
            "#!/bin/sh\necho 'unrecognized subcommand' >&2\nexit 2\n",
        )
        .unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();

        let hook = StopHook::new(&old, &world.ledger).unwrap();
        let out = Command::new("sh")
            .args(["-c", &hook.command])
            .output()
            .unwrap();

        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stderr).contains("unrecognized"));
    }

    #[test]
    fn a_users_settings_file_is_merged_and_passed_once() {
        let world = world();
        let user = world.repo.join("user.json");
        let theirs = serde_json::json!({"type": "command", "command": "notify"});
        let user_settings = serde_json::json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [theirs]}],
                "SessionStart": [{"hooks": [theirs]}],
            },
        });
        fs::write(&user, user_settings.to_string()).unwrap();
        let expected = serde_json::json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [theirs]}, stop_hook(&world)],
                "SessionStart": [{"hooks": [theirs]}],
            },
        });

        for flag in [
            vec!["--settings", user.to_str().unwrap(), "--verbose"],
            vec![&format!("--settings={}", user.display()), "--verbose"],
            // claude resolves a relative path from the member's cwd.
            vec!["--settings", "user.json", "--verbose"],
        ] {
            let (args, settings) = spawn_with(&world, &flag).unwrap();

            let flags = args.iter().filter(|a| a.starts_with("--settings")).count();
            assert_eq!(flags, 1, "{flag:?}");
            assert_eq!(args[0], "--verbose");
            assert_eq!(settings, expected, "{flag:?}");
        }
    }

    #[test]
    fn inline_json_settings_are_merged_too() {
        let world = world();

        let (args, settings) = spawn_with(&world, &[r#"--settings={"model": "opus"}"#]).unwrap();

        assert_eq!(args.len(), 2);
        assert_eq!(
            settings,
            serde_json::json!({"model": "opus", "hooks": {"Stop": [stop_hook(&world)]}})
        );
    }

    #[test]
    fn unusable_settings_fail_the_start_and_record_nothing() {
        let world = world();
        fs::write(world.repo.join("list.json"), "[1]").unwrap();
        fs::write(world.repo.join("hooks.json"), r#"{"hooks": []}"#).unwrap();

        for (args, error) in [
            (vec!["--settings"], "--settings needs a value"),
            (vec![r#"--settings={"model": "#], "parse --settings JSON"),
            (vec!["--settings", "missing.json"], "read --settings"),
            (vec!["--settings", "list.json"], "is not a JSON object"),
            (
                vec!["--settings", "hooks.json"],
                "hooks is not a JSON object",
            ),
        ] {
            let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
            let mut ledger = MemLedger::default();
            let req = request(&world, &args);

            let err = spawn(&hooked(&herdr, &world), &mut ledger, &req, Some("w1")).unwrap_err();

            assert!(format!("{err:#}").contains(error), "{args:?}: {err:#}");
            assert!(herdr.started().is_empty(), "{args:?}");
            assert!(herdr.tabs().is_empty(), "{args:?}: tab left open");
            assert!(ledger.0.is_empty(), "{args:?}");
        }
    }
}
