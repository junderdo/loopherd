//! Opening the herd view in a pane beside the shepherd's, the one its
//! handoff record names, rather than in the plugin's popup.

use std::path::Path;

use anyhow::{Result, bail};

use crate::herdr::{Herdr, Placement};
use crate::process::{self, Procs};
use crate::shepherd_resume::{self, Place};

const VIEW: &str = "view";
/// Set on a view opened anywhere but beside the shepherd, saying why.
pub const NOTE: &str = "LOOPHERD_VIEW_NOTE";
/// The view's share of the width it splits off.
const SHARE: f64 = 0.4;

#[derive(Debug, PartialEq)]
pub enum Opened {
    BesideShepherd { pane: String },
    Beside { pane: String, why: String },
    Popup { why: String },
}

/// Opens the view right of the shepherd's recorded pane in `state`, else
/// right of `current`, the pane the action ran from, else in the popup.
pub fn open(
    herdr: &dyn Herdr,
    procs: &dyn Procs,
    state: &Path,
    current: Option<&str>,
) -> Result<Opened> {
    let why = match shepherd_resume::recorded_place(state) {
        None => "no shepherd recorded yet; its first handoff records its pane".to_string(),
        Some(place) if herdr.socket().as_ref() != Some(&place.socket) => format!(
            "the recorded shepherd ({}) is in another herdr session",
            place.pane
        ),
        Some(place) => match beside_shepherd(herdr, procs, &place) {
            Ok(()) => return Ok(Opened::BesideShepherd { pane: place.pane }),
            Err(e) => format!("not beside the shepherd's pane {}: {e:#}", place.pane),
        },
    };
    let why = match current {
        Some(pane) => match herdr.open_plugin_pane(VIEW, &right_of(pane), &[(NOTE, &why)]) {
            Ok(()) => {
                return Ok(Opened::Beside {
                    pane: pane.into(),
                    why,
                });
            }
            Err(e) => format!("{why}; no split beside {pane} either: {e:#}"),
        },
        None => why,
    };
    herdr.open_plugin_pane(VIEW, &Placement::Manifest, &[(NOTE, &why)])?;
    Ok(Opened::Popup { why })
}

/// A pane id outlives its pane, so the shepherd's Claude must still run
/// under the pane's shell.
fn beside_shepherd(herdr: &dyn Herdr, procs: &dyn Procs, place: &Place) -> Result<()> {
    let shell = herdr.shell_pid(&place.pane)?;
    let claude = process::live(procs, Some(place.process));
    if !shell
        .zip(claude)
        .is_some_and(|(shell, claude)| process::is_ancestor(procs, shell, claude.pid))
    {
        bail!(
            "it no longer runs the shepherd's Claude (pid {})",
            place.process.pid
        );
    }
    herdr.open_plugin_pane(VIEW, &right_of(&place.pane), &[])
}

fn right_of(pane: &str) -> Placement {
    Placement::RightOf {
        pane: pane.into(),
        share: SHARE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::ledger::Process;
    use crate::process::{FakeProcs, PANE_SHELL};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// A herdr whose one pane's shell is [`FakeProcs::pane`]'s, under the
    /// shepherd's Claude, pid 30.
    fn herdr_with_tab() -> (FakeHerdr, String) {
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let pane = herdr
            .create_tab("w1", Path::new("/src"), "shepherd")
            .unwrap()
            .pane_id;
        herdr.set_shell_pid(&pane, Some(PANE_SHELL.pid));
        (herdr, pane)
    }

    fn record_shepherd(state: &Path, pane: &str, socket: &str) {
        let place = shepherd_resume::Place {
            pane: pane.into(),
            socket: PathBuf::from(socket),
            process: Process {
                pid: 30,
                start: 300,
            },
        };
        shepherd_resume::record(state, &place, "s-1", SystemTime::now()).unwrap();
    }

    fn note(herdr: &FakeHerdr) -> String {
        let [(_, _, env)] = herdr.plugin_panes().try_into().unwrap();
        let [(name, why)] = env.try_into().unwrap();
        assert_eq!(name, NOTE);
        why
    }

    fn beside_current_why(opened: Opened, current: &str) -> String {
        let Opened::Beside { pane, why } = opened else {
            panic!("{opened:?}");
        };
        assert_eq!(pane, current);
        why
    }

    #[test]
    fn the_view_opens_right_of_the_recorded_shepherd_pane() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone()
            }
        );
        assert_eq!(
            herdr.plugin_panes(),
            [(VIEW.to_string(), right_of(&shepherd), vec![])]
        );
    }

    #[test]
    fn a_used_record_still_names_the_shepherds_pane() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        let path = state.path().join("shepherd.json");
        let used = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"used\": false", "\"used\": true");
        std::fs::write(&path, used).unwrap();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(opened, Opened::BesideShepherd { pane: shepherd });
    }

    #[test]
    fn with_no_shepherd_recorded_the_view_opens_right_of_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert_eq!(herdr.plugin_panes()[0].1, right_of(&current));
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_shepherd_pane_gone_opens_right_of_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        record_shepherd(state.path(), "w1:p99", FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("w1:p99 not found"), "{why}");
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_reused_pane_id_without_the_shepherds_claude_counts_as_gone() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        herdr.set_shell_pid(&shepherd, Some(21));
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no longer runs the shepherd"), "{why}");
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_shepherd_claude_that_ended_counts_as_gone() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let restarted = FakeProcs::restarted_pane();
        let opened = open(&herdr, &restarted, state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no longer runs the shepherd"), "{why}");
    }

    #[test]
    fn a_shepherd_in_another_herdr_session_counts_as_not_here() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        record_shepherd(state.path(), &current, "/run/other/herdr.sock");

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("another herdr session"), "{why}");
    }

    #[test]
    fn with_no_shepherd_and_no_current_pane_the_view_opens_in_the_popup_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        let Opened::Popup { why } = opened else {
            panic!("{opened:?}");
        };
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert_eq!(
            herdr.popups(),
            [(VIEW.to_string(), vec![(NOTE.to_string(), why)])]
        );
    }

    #[test]
    fn a_failed_split_beside_the_current_pane_falls_back_to_the_popup_with_both_reasons() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some("w1:p77")).unwrap();

        let Opened::Popup { why } = opened else {
            panic!("{opened:?}");
        };
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert!(why.contains("w1:p77 not found"), "{why}");
        assert_eq!(
            herdr.popups(),
            [(VIEW.to_string(), vec![(NOTE.to_string(), why)])]
        );
    }
}
