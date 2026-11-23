//! Bringing up the shepherd from the herd view: its pane when it's alive,
//! else a new one split off the view's on the side `view.layout` picks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::context::SHEPHERD;
use crate::herdr::{Herdr, wait_for_shell_prompt};
use crate::process::{self, Procs};
use crate::shepherd_resume::{self, Place};
use crate::view_pane::{self, Layout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summoned {
    Focused(String),
    Started(String),
    /// Started in the pane, which then couldn't be focused, for the reason given.
    Unfocused(String, String),
    /// The recorded shepherd's Claude still runs, but not in a pane of this
    /// herdr that the view can focus, so nothing starts.
    Elsewhere(Place),
    /// No live shepherd, and the view has no pane to split, as in a popup.
    NoPane,
}

/// The view's own pane, which a shepherd started from the view splits off,
/// in `cwd`, with `env` for loopherd's state and config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewPane {
    pub pane: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_POLL: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(60);

const START_PROMPT: &str = "You're the loopherd shepherd. Run `loopherd --skill` and follow its \
     Shepherd section: feed the herds, watch them, verify members' work, and clean up after merges.";

/// Focuses the live shepherd, or starts one beside the view's pane as
/// `layout` places it; `None` is a view with no pane of its own. A failed
/// start closes the pane it split.
pub fn summon(
    herdr: &dyn Herdr,
    procs: &dyn Procs,
    state: &Path,
    view: Option<&ViewPane>,
    layout: Layout,
) -> Result<Summoned> {
    let agents = herdr.agents()?;
    let named = agents.iter().find(|a| a.name.as_deref() == Some(SHEPHERD));
    let live = match (named, shepherd_resume::recorded_place_and_shell(state)) {
        (Some(agent), _) => Some(agent.pane_id.clone()),
        (None, Some((place, shell))) => {
            let here = herdr.socket().as_deref() == Some(place.socket.as_path())
                && agents.iter().any(|a| a.pane_id == place.pane);
            let claude_runs = process::live(procs, Some(place.process)).is_some();
            // The shell outlives a Claude restarted in it; a pane id reused later runs another.
            let in_its_shell = match process::live(procs, shell) {
                Some(shell) if here && !claude_runs => {
                    herdr.shell_pid(&place.pane)? == Some(shell.pid)
                }
                _ => false,
            };
            match (here && (claude_runs || in_its_shell), claude_runs) {
                (true, _) => Some(place.pane),
                (false, true) => return Ok(Summoned::Elsewhere(place)),
                (false, false) => None,
            }
        }
        (None, None) => None,
    };
    if let Some(pane) = live {
        herdr.focus(&pane)?;
        return Ok(Summoned::Focused(pane));
    }
    let Some(view) = view else {
        return Ok(Summoned::NoPane);
    };
    let env: Vec<(&str, &str)> = view
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let pane = herdr.split(
        &view.pane,
        layout.shepherd_side(),
        1.0 - view_pane::SHARE,
        &view.cwd,
        &env,
    )?;
    if let Err(e) = start_in(herdr, state, &pane) {
        let _ = herdr.close_pane(&pane);
        return Err(e);
    }
    match herdr.focus(&pane) {
        Ok(()) => Ok(Summoned::Started(pane)),
        Err(e) => Ok(Summoned::Unfocused(pane, format!("{e:#}"))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    Switched(Layout),
    /// Nothing moved, for the reason given.
    Unswitched(String),
}

/// Moves the live shepherd's pane, when it shares the view's tab, from `from`
/// to the other layout, without restarting either pane.
pub fn switch_layout(herdr: &dyn Herdr, view: Option<&ViewPane>, from: Layout) -> Result<Switch> {
    let Some(view) = view else {
        return Ok(Switch::Unswitched(
            "the view is in a popup, with no split to switch".into(),
        ));
    };
    let agents = herdr.agents()?;
    let Some(shepherd) = agents.iter().find(|a| a.name.as_deref() == Some(SHEPHERD)) else {
        return Ok(Switch::Unswitched(
            "no live shepherd to switch the layout with".into(),
        ));
    };
    if shepherd.tab_id != herdr.pane(&view.pane)?.tab_id {
        return Ok(Switch::Unswitched(format!(
            "the shepherd's pane {} isn't in the view's tab",
            shepherd.pane_id
        )));
    }
    let to = from.other();
    herdr
        .dock(
            &shepherd.pane_id,
            &view.pane,
            to.shepherd_side(),
            1.0 - view_pane::SHARE,
        )
        .with_context(|| format!("move the shepherd's pane {}", shepherd.pane_id))?;
    Ok(Switch::Switched(to))
}

/// Starts the shepherd in `pane`, giving the handoff back when the start fails.
fn start_in(herdr: &dyn Herdr, state: &Path, pane: &str) -> Result<()> {
    wait_for_shell_prompt(herdr, pane, SHELL_TIMEOUT, SHELL_POLL)?;
    let resume = shepherd_resume::new_shepherd_resume(state, herdr.socket().as_deref())?;
    let prompt = resume
        .as_ref()
        .map_or(START_PROMPT, |r| &r.prompt)
        .to_string();
    // A blocked start waits on a dialog in the pane, which focusing shows.
    let Err(e) = herdr.start_claude(SHEPHERD, pane, START_TIMEOUT, &[prompt]) else {
        return Ok(());
    };
    match resume.map(|r| r.undo(state)).transpose() {
        Ok(_) => Err(e),
        Err(undo) => Err(e.context(format!("and the handoff wasn't given back: {undo:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Side;
    use crate::herdr::fake::Startup;
    use crate::herdr::fake::{Dock, FAKE_SOCKET, FakeHerdr, Split};
    use crate::ledger::Process;
    use crate::process::{FakeProcs, PANE_SHELL};

    fn view_pane(pane: &str) -> ViewPane {
        ViewPane {
            pane: pane.into(),
            cwd: "/repo".into(),
            env: vec![("XDG_STATE_HOME".into(), "/state".into())],
        }
    }

    /// The Claude process [`recorded_at`] records, still running.
    fn shepherd_alive() -> FakeProcs {
        let mut procs = FakeProcs::default();
        procs.add(4242, "/usr/bin/claude", 1, 7);
        procs
    }

    fn recorded(state: &Path, pane: &str, socket: &str) {
        recorded_at(state, pane, socket, "2026-11-19T09:00:00Z");
    }

    fn recorded_at(state: &Path, pane: &str, socket: &str, at: &str) {
        let place = Place {
            pane: pane.into(),
            socket: socket.into(),
            process: Process {
                pid: 4242,
                start: 7,
            },
        };
        let at = humantime::parse_rfc3339(at).unwrap();
        shepherd_resume::record(state, &place, None, "s1", at).unwrap();
    }

    fn handed_off(state: &Path, written: &str) -> std::path::PathBuf {
        let path = state.join("handoffs/shepherd/handoff.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## State\n").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(humantime::parse_rfc3339(written).unwrap())
            .unwrap();
        path
    }

    /// The first prompt of the shepherd `summon` starts below a fresh view
    /// pane, with the recorded Claude gone.
    fn started_prompt(state: &Path) -> String {
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);
        summon(
            &herdr,
            &FakeProcs::default(),
            state,
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();
        let [started] = herdr.started().try_into().unwrap();
        let [prompt] = started.args.try_into().unwrap();
        prompt
    }

    #[test]
    fn a_live_agent_named_shepherd_is_focused_and_nothing_starts() {
        let state = tempfile::tempdir().unwrap();
        let herdr =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(Some("shepherd"), None, "w1:p5");

        let summoned = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            Some(&view_pane("w1:p1")),
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(summoned, Summoned::Focused("w1:p5".into()));
        assert_eq!(herdr.focused(), vec!["w1:p5"]);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn the_recorded_shepherd_pane_with_a_live_agent_is_focused() {
        let state = tempfile::tempdir().unwrap();
        recorded(state.path(), "w1:p7", FAKE_SOCKET);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s2"), "w1:p7");

        let summoned = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            Some(&view_pane("w1:p1")),
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(summoned, Summoned::Focused("w1:p7".into()));
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn side_by_side_a_shepherd_starts_left_of_the_view_with_the_view_keeping_two_fifths() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::SideBySide,
        )
        .unwrap();

        let [split] = herdr.splits().try_into().unwrap();
        assert_eq!(
            (split.from.as_str(), split.side, split.share),
            (view.as_str(), Side::Left, 600)
        );
        assert_eq!(summoned, Summoned::Started(split.pane.clone()));
        assert_eq!(herdr.focused(), vec![split.pane]);
    }

    #[test]
    fn with_no_live_shepherd_it_starts_below_the_view_and_a_second_press_focuses_it() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        let [split] = herdr.splits().try_into().unwrap();
        let Split { pane, .. } = &split;
        assert_eq!(
            split,
            Split {
                from: view.clone(),
                pane: pane.clone(),
                side: Side::Below,
                share: 600,
                cwd: "/repo".into(),
                env: vec![("XDG_STATE_HOME".into(), "/state".into())],
            }
        );
        assert_eq!(summoned, Summoned::Started(pane.clone()));
        let [started] = herdr.started().try_into().unwrap();
        assert_eq!(
            (started.name.as_str(), started.pane_id.as_str()),
            ("shepherd", pane.as_str())
        );
        let [prompt] = started.args.try_into().unwrap();
        assert!(prompt.contains("`loopherd --skill`"), "{prompt}");
        assert!(prompt.contains("Shepherd section"), "{prompt}");
        assert_eq!(herdr.focused(), vec![pane.clone()]);

        let again = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(again, Summoned::Focused(pane.clone()));
        assert_eq!(herdr.started().len(), 1);
    }

    #[test]
    fn without_a_view_pane_it_starts_nothing() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "main")]);

        let summoned = summon(
            &herdr,
            &shepherd_alive(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

        assert_eq!(summoned, Summoned::NoPane);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn a_live_recorded_claude_this_view_cant_focus_starts_nothing_and_keeps_the_handoff() {
        for (record_socket, agent_pane) in
            [("/run/other/herdr.sock", "w1:p7"), (FAKE_SOCKET, "w1:p8")]
        {
            let state = tempfile::tempdir().unwrap();
            recorded_at(state.path(), "w1:p7", record_socket, "2026-11-19T09:00:00Z");
            let handoff = handed_off(state.path(), "2026-11-19T09:05:00Z");
            let herdr = FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, None, agent_pane);
            let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

            let summoned = summon(
                &herdr,
                &shepherd_alive(),
                state.path(),
                Some(&view_pane(&view)),
                Layout::Stacked,
            )
            .unwrap();

            let Summoned::Elsewhere(place) = summoned else {
                panic!("{record_socket} {agent_pane}: {summoned:?}");
            };
            assert_eq!(
                (place.process.pid, place.socket.to_str()),
                (4242, Some(record_socket))
            );
            assert!(herdr.splits().is_empty() && herdr.started().is_empty());
            assert!(handoff.exists());
            let record = std::fs::read_to_string(state.path().join("shepherd.json")).unwrap();
            assert!(record.contains(r#""used": false"#), "{record}");
        }
    }

    #[test]
    fn a_handoff_newer_than_the_record_is_resumed_from_once() {
        let state = tempfile::tempdir().unwrap();
        recorded_at(state.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");
        let handoff = handed_off(state.path(), "2026-11-19T09:05:00Z");

        let prompt = started_prompt(state.path());

        let moved = state.path().join("handoffs/shepherd/prev.md");
        assert!(prompt.contains("resuming after your handoff"), "{prompt}");
        assert!(prompt.contains(&moved.display().to_string()), "{prompt}");
        assert!(!handoff.exists() && moved.exists());
        assert!(started_prompt(state.path()).contains("Shepherd section"));
    }

    #[test]
    fn a_handoff_with_no_record_or_older_than_the_record_is_not_resumed_from() {
        let unrecorded = tempfile::tempdir().unwrap();
        handed_off(unrecorded.path(), "2026-11-19T09:05:00Z");
        let stale = tempfile::tempdir().unwrap();
        handed_off(stale.path(), "2026-11-19T08:55:00Z");
        recorded_at(stale.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");

        assert!(started_prompt(unrecorded.path()).contains("Shepherd section"));
        assert!(started_prompt(stale.path()).contains("Shepherd section"));
    }

    #[test]
    fn a_handoff_recorded_on_another_herdr_is_not_resumed_from_or_marked_used() {
        let state = tempfile::tempdir().unwrap();
        recorded_at(
            state.path(),
            "w1:p7",
            "/run/other/herdr.sock",
            "2026-11-19T09:00:00Z",
        );
        let handoff = handed_off(state.path(), "2026-11-19T09:05:00Z");

        assert!(started_prompt(state.path()).contains("Shepherd section"));
        assert!(handoff.exists());
        let record = std::fs::read_to_string(state.path().join("shepherd.json")).unwrap();
        assert!(record.contains(r#""used": false"#), "{record}");
    }

    #[test]
    fn a_handoff_after_the_recorded_claude_moved_to_another_session_is_not_resumed_from() {
        let state = tempfile::tempdir().unwrap();
        recorded_at(state.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");
        handed_off(state.path(), "2026-11-19T09:05:00Z");
        let place = Place {
            pane: "w1:p7".into(),
            socket: FAKE_SOCKET.into(),
            process: Process {
                pid: 4242,
                start: 7,
            },
        };
        let cleared = br#"{"source":"clear","session_id":"s2"}"#;
        shepherd_resume::session_started(state.path(), &place, cleared, &|s| s.into()).unwrap();

        assert!(started_prompt(state.path()).contains("Shepherd section"));
    }

    #[test]
    fn the_recorded_pane_whose_claude_has_exited_is_no_live_shepherd() {
        let state = tempfile::tempdir().unwrap();
        recorded(state.path(), "w1:p7", FAKE_SOCKET);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s9"), "w1:p7");
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);
        let mut restarted = FakeProcs::default();
        restarted.add(4242, "/usr/bin/claude", 1, 8);

        let summoned = summon(
            &herdr,
            &restarted,
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        assert!(matches!(summoned, Summoned::Started(_)), "{summoned:?}");
    }

    /// A herdr whose pane `w1:p1` runs [`PANE_SHELL`], and a record of the
    /// shepherd's Claude (pid 4242) there under that shell.
    fn recorded_under_the_pane_shell(state: &Path, herdr: FakeHerdr) -> (FakeHerdr, String) {
        let pane = herdr
            .create_tab("w1", Path::new("/repo"), "shepherd")
            .unwrap()
            .pane_id;
        assert_eq!(pane, "w1:p1");
        herdr.set_shell_pid(&pane, Some(PANE_SHELL.pid));
        let place = Place {
            pane: pane.clone(),
            socket: FAKE_SOCKET.into(),
            process: Process {
                pid: 4242,
                start: 7,
            },
        };
        let at = humantime::parse_rfc3339("2026-11-19T09:00:00Z").unwrap();
        shepherd_resume::record(state, &place, Some(PANE_SHELL), "s1", at).unwrap();
        (herdr, pane)
    }

    /// The pane's shell still running, the recorded Claude (pid 4242) gone.
    fn only_the_pane_shell() -> FakeProcs {
        let mut procs = FakeProcs::default();
        procs.add(PANE_SHELL.pid, "/bin/bash", 10, PANE_SHELL.start);
        procs
    }

    #[test]
    fn a_shepherd_claude_restarted_in_its_recorded_pane_is_focused_and_nothing_starts() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = recorded_under_the_pane_shell(
            state.path(),
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s2"), "w1:p1"),
        );
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(
            &herdr,
            &only_the_pane_shell(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(summoned, Summoned::Focused(shepherd.clone()));
        assert_eq!(herdr.focused(), vec![shepherd]);
        assert!(herdr.splits().is_empty() && herdr.started().is_empty());
    }

    #[test]
    fn the_recorded_shell_left_at_a_bare_prompt_is_no_live_shepherd() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, _) =
            recorded_under_the_pane_shell(state.path(), FakeHerdr::new(&[("w1", "main")]));
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(
            &herdr,
            &only_the_pane_shell(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        assert!(matches!(summoned, Summoned::Started(_)), "{summoned:?}");
    }

    #[test]
    fn a_recorded_pane_whose_shell_changed_or_restarted_is_no_live_shepherd() {
        let mut replaced = only_the_pane_shell();
        replaced.add(21, "/bin/bash", 10, 210);
        let mut restarted = FakeProcs::default();
        restarted.add(PANE_SHELL.pid, "/bin/bash", 10, 910);
        for (shell_pid, procs) in [(21, replaced), (PANE_SHELL.pid, restarted)] {
            let state = tempfile::tempdir().unwrap();
            let (herdr, shepherd) = recorded_under_the_pane_shell(
                state.path(),
                FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s9"), "w1:p1"),
            );
            herdr.set_shell_pid(&shepherd, Some(shell_pid));
            let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

            let summoned = summon(
                &herdr,
                &procs,
                state.path(),
                Some(&view_pane(&view)),
                Layout::Stacked,
            )
            .unwrap();

            assert!(
                matches!(summoned, Summoned::Started(_)),
                "{shell_pid}: {summoned:?}"
            );
        }
    }

    #[test]
    fn a_recorded_pane_whose_shell_herdr_cant_read_fails_and_starts_nothing() {
        let state = tempfile::tempdir().unwrap();
        let mut herdr =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s2"), "w1:p1");
        herdr.fail_shell_pid = true;
        let (herdr, _) = recorded_under_the_pane_shell(state.path(), herdr);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let failed = summon(
            &herdr,
            &only_the_pane_shell(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        );

        let error = format!("{:#}", failed.unwrap_err());
        assert!(error.contains("process-info failed"), "{error}");
        assert!(herdr.splits().is_empty() && herdr.started().is_empty());
    }

    #[test]
    fn a_failed_start_closes_its_pane_and_leaves_the_handoff_for_the_next_press() {
        let state = tempfile::tempdir().unwrap();
        recorded_at(state.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");
        let handoff = handed_off(state.path(), "2026-11-19T09:05:00Z");
        let mut herdr = FakeHerdr::new(&[("w1", "main")]);
        herdr.startup = Startup::Fails;
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let failed = summon(
            &herdr,
            &FakeProcs::default(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        );

        assert!(failed.is_err());
        let [split] = herdr.splits().try_into().unwrap();
        assert!(!herdr.has_pane(&split.pane));
        assert!(handoff.exists());
        assert!(started_prompt(state.path()).contains("resuming after your handoff"));
    }

    #[test]
    fn a_focus_that_fails_after_the_start_keeps_the_running_shepherd() {
        let state = tempfile::tempdir().unwrap();
        let mut herdr = FakeHerdr::new(&[("w1", "main")]);
        herdr.fail_focus = true;
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(
            &herdr,
            &FakeProcs::default(),
            state.path(),
            Some(&view_pane(&view)),
            Layout::Stacked,
        )
        .unwrap();

        let [split] = herdr.splits().try_into().unwrap();
        assert_eq!(
            summoned,
            Summoned::Unfocused(split.pane.clone(), "focus failed".into())
        );
        assert!(herdr.has_pane(&split.pane));
        assert_eq!(herdr.started().len(), 1);
    }

    /// A herdr whose tab `w1:t1` holds the live shepherd, `w1:p1`, and a view pane.
    fn shepherd_beside_view() -> (FakeHerdr, String) {
        let herdr =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(Some("shepherd"), None, "w1:p1");
        let shepherd = herdr.add_tab("w1", "shepherd", Path::new("/repo"), true);
        assert_eq!(shepherd, "w1:p1");
        let view = herdr.split("w1:t1", Path::new("/repo"));
        (herdr, view)
    }

    #[test]
    fn switching_from_side_by_side_docks_the_shepherd_below_the_view_keeping_two_fifths() {
        let (herdr, view) = shepherd_beside_view();

        let switched = switch_layout(&herdr, Some(&view_pane(&view)), Layout::SideBySide).unwrap();

        assert_eq!(switched, Switch::Switched(Layout::Stacked));
        let [dock] = herdr.docks().try_into().unwrap();
        assert_eq!(
            dock,
            Dock {
                pane: "w1:p1".into(),
                target: view,
                side: Side::Below,
                share: 600,
            }
        );
        assert_eq!(herdr.started().len(), 0);
        assert_eq!(herdr.focused(), Vec::<String>::new());
    }

    #[test]
    fn switching_from_stacked_docks_the_shepherd_left_of_the_view() {
        let (herdr, view) = shepherd_beside_view();

        let switched = switch_layout(&herdr, Some(&view_pane(&view)), Layout::Stacked).unwrap();

        assert_eq!(switched, Switch::Switched(Layout::SideBySide));
        let [dock] = herdr.docks().try_into().unwrap();
        assert_eq!((dock.side, dock.share), (Side::Left, 600));
    }

    #[test]
    fn with_no_shepherd_pane_in_the_views_tab_nothing_moves_and_it_says_why() {
        let elsewhere =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(Some("shepherd"), None, "w1:p9");
        let none = FakeHerdr::new(&[("w1", "main")]);
        for (herdr, says) in [
            (
                &elsewhere,
                "the shepherd's pane w1:p9 isn't in the view's tab",
            ),
            (&none, "no live shepherd"),
        ] {
            let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

            let switched =
                switch_layout(herdr, Some(&view_pane(&view)), Layout::SideBySide).unwrap();

            let Switch::Unswitched(why) = switched else {
                panic!("{switched:?}");
            };
            assert!(why.contains(says), "{why}");
            assert_eq!(herdr.mutations(), 0);
        }
    }

    #[test]
    fn a_view_with_no_pane_of_its_own_switches_nothing() {
        let herdr =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(Some("shepherd"), None, "w1:p1");

        let switched = switch_layout(&herdr, None, Layout::SideBySide).unwrap();

        let Switch::Unswitched(why) = switched else {
            panic!("{switched:?}");
        };
        assert!(why.contains("popup"), "{why}");
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn a_failing_move_is_an_error_naming_it() {
        let (mut herdr, view) = shepherd_beside_view();
        herdr.fail_dock = true;

        let err = switch_layout(&herdr, Some(&view_pane(&view)), Layout::SideBySide).unwrap_err();

        let err = format!("{err:#}");
        assert!(err.contains("w1:p1"), "{err}");
        assert!(err.contains("dock failed"), "{err}");
    }
}
