//! Bringing up the shepherd from the herd view: its pane when it's alive,
//! else a new one split below the view's.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::context::SHEPHERD;
use crate::herdr::{Herdr, wait_for_shell_prompt};
use crate::shepherd_resume;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summoned {
    Focused(String),
    Started(String),
    /// No live shepherd, and the view has no pane to split, as in a popup.
    NoPane,
}

/// Where a shepherd started from the view goes: split below the view's
/// `pane`, in `cwd`, with `env` for loopherd's state and config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Below {
    pub pane: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_POLL: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(60);

const START_PROMPT: &str = "You're the loopherd shepherd. Run `loopherd --skill` and follow its \
     Shepherd section: feed the herds, watch them, verify members' work, and clean up after merges.";

/// Focuses the live shepherd, or starts one `below` the view; `None` is a
/// view with no pane of its own.
pub fn summon(herdr: &dyn Herdr, state: &Path, below: Option<&Below>) -> Result<Summoned> {
    if let Some(pane) = live_shepherd(herdr, state)? {
        herdr.focus(&pane)?;
        return Ok(Summoned::Focused(pane));
    }
    let Some(below) = below else {
        return Ok(Summoned::NoPane);
    };
    let env: Vec<(&str, &str)> = below
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let pane = herdr.split_down(&below.pane, &below.cwd, &env)?;
    wait_for_shell_prompt(herdr, &pane, SHELL_TIMEOUT, SHELL_POLL)?;
    let prompt =
        shepherd_resume::new_shepherd_resume(state)?.unwrap_or_else(|| START_PROMPT.into());
    // A blocked start waits on a dialog in the pane, which focusing shows.
    herdr.start_claude(SHEPHERD, &pane, START_TIMEOUT, &[prompt])?;
    herdr.focus(&pane)?;
    Ok(Summoned::Started(pane))
}

/// The pane of an agent named for the shepherd, or of the agent in the pane
/// the last handoff record names on this herdr's socket.
fn live_shepherd(herdr: &dyn Herdr, state: &Path) -> Result<Option<String>> {
    let agents = herdr.agents()?;
    let recorded = shepherd_resume::recorded_place(state)
        .filter(|place| herdr.socket().as_deref() == Some(place.socket.as_path()));
    let live = agents.iter().find(|a| {
        a.name.as_deref() == Some(SHEPHERD)
            || recorded.as_ref().is_some_and(|p| p.pane == a.pane_id)
    });
    Ok(live.map(|a| a.pane_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr, Split};
    use crate::ledger::Process;
    use crate::shepherd_resume::{self, Place};

    fn below(pane: &str) -> Below {
        Below {
            pane: pane.into(),
            cwd: "/repo".into(),
            env: vec![("XDG_STATE_HOME".into(), "/state".into())],
        }
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
        shepherd_resume::record(state, &place, "s1", at).unwrap();
    }

    fn handed_off(state: &Path, written: &str) -> std::path::PathBuf {
        let path = state.join("handoffs/shepherd.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "## State\n").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(humantime::parse_rfc3339(written).unwrap())
            .unwrap();
        path
    }

    /// The first prompt of the shepherd `summon` starts below a fresh view pane.
    fn started_prompt(state: &Path) -> String {
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);
        summon(&herdr, state, Some(&below(&view))).unwrap();
        let [started] = herdr.started().try_into().unwrap();
        let [prompt] = started.args.try_into().unwrap();
        prompt
    }

    #[test]
    fn a_live_agent_named_shepherd_is_focused_and_nothing_starts() {
        let state = tempfile::tempdir().unwrap();
        let herdr =
            FakeHerdr::new(&[("w1", "main")]).with_live_agent(Some("shepherd"), None, "w1:p5");

        let summoned = summon(&herdr, state.path(), Some(&below("w1:p1"))).unwrap();

        assert_eq!(summoned, Summoned::Focused("w1:p5".into()));
        assert_eq!(herdr.focused(), vec!["w1:p5"]);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn the_recorded_shepherd_pane_with_a_live_agent_is_focused() {
        let state = tempfile::tempdir().unwrap();
        recorded(state.path(), "w1:p7", FAKE_SOCKET);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, Some("s2"), "w1:p7");

        let summoned = summon(&herdr, state.path(), Some(&below("w1:p1"))).unwrap();

        assert_eq!(summoned, Summoned::Focused("w1:p7".into()));
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn with_no_live_shepherd_it_starts_below_the_view_and_a_second_press_focuses_it() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "main")]);
        let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

        let summoned = summon(&herdr, state.path(), Some(&below(&view))).unwrap();

        let [split] = herdr.splits().try_into().unwrap();
        let Split { pane, .. } = &split;
        assert_eq!(
            split,
            Split {
                from: view.clone(),
                pane: pane.clone(),
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

        let again = summon(&herdr, state.path(), Some(&below(&view))).unwrap();

        assert_eq!(again, Summoned::Focused(pane.clone()));
        assert_eq!(herdr.started().len(), 1);
    }

    #[test]
    fn without_a_view_pane_it_starts_nothing() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "main")]);

        let summoned = summon(&herdr, state.path(), None).unwrap();

        assert_eq!(summoned, Summoned::NoPane);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn a_record_from_another_herdr_or_a_dead_pane_is_no_live_shepherd() {
        for (record_socket, agent_pane) in
            [("/run/other/herdr.sock", "w1:p7"), (FAKE_SOCKET, "w1:p8")]
        {
            let state = tempfile::tempdir().unwrap();
            recorded(state.path(), "w1:p7", record_socket);
            let herdr = FakeHerdr::new(&[("w1", "main")]).with_live_agent(None, None, agent_pane);
            let view = herdr.add_tab("w1", "view", Path::new("/repo"), true);

            let summoned = summon(&herdr, state.path(), Some(&below(&view))).unwrap();

            assert!(
                matches!(summoned, Summoned::Started(_)),
                "{record_socket} {agent_pane}"
            );
        }
    }

    #[test]
    fn a_handoff_newer_than_the_record_is_resumed_from_once() {
        let state = tempfile::tempdir().unwrap();
        recorded_at(state.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");
        let handoff = handed_off(state.path(), "2026-11-19T09:05:00Z");

        let prompt = started_prompt(state.path());

        let moved = state.path().join("handoffs/shepherd.prev.md");
        assert!(prompt.contains("resuming after your handoff"), "{prompt}");
        assert!(prompt.contains(&moved.display().to_string()), "{prompt}");
        assert!(!handoff.exists() && moved.exists());
        assert!(started_prompt(state.path()).contains("Shepherd section"));
    }

    #[test]
    fn a_handoff_with_no_record_is_resumed_from_and_one_older_than_the_record_is_not() {
        let unrecorded = tempfile::tempdir().unwrap();
        handed_off(unrecorded.path(), "2026-11-19T09:05:00Z");
        let stale = tempfile::tempdir().unwrap();
        handed_off(stale.path(), "2026-11-19T08:55:00Z");
        recorded_at(stale.path(), "w1:p7", FAKE_SOCKET, "2026-11-19T09:00:00Z");

        assert!(started_prompt(unrecorded.path()).contains("resuming after your handoff"));
        assert!(started_prompt(stale.path()).contains("Shepherd section"));
    }
}
