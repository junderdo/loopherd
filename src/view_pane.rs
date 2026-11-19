//! Opening the herd view in a pane beside the shepherd's, the one its
//! handoff record names, rather than in the plugin's popup.

use std::path::Path;

use anyhow::{Result, bail};

use crate::herdr::{Herdr, Pane, Placement};
use crate::process::{self, Procs};
use crate::shepherd_resume::{self, Place};

const VIEW: &str = "view";
/// The view pane's title in the plugin manifest, which herdr shows as its label.
const VIEW_TITLE: &str = "loopherd view";
/// Set on a view opened anywhere but beside the shepherd, saying why.
pub const NOTE: &str = "LOOPHERD_VIEW_NOTE";
/// The view's share of the width it splits off.
const SHARE: f64 = 0.4;

#[derive(Debug, PartialEq)]
pub enum Opened {
    BesideShepherd { pane: String, how: How },
    Beside { pane: String, why: String, how: How },
    Popup { why: String },
}

#[derive(Debug, PartialEq)]
pub enum How {
    /// Split off, leaving focus where it was.
    Opened,
    /// Split off and focused, the pane's tab being out of the human's sight.
    OpenedFocused,
    /// A view already open in the pane's tab, focused instead of stacking another.
    Focused { view: String },
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
            Ok(how) => {
                return Ok(Opened::BesideShepherd {
                    pane: place.pane,
                    how,
                });
            }
            Err(e) => format!("not beside the shepherd's pane {}: {e:#}", place.pane),
        },
    };
    let why = match current {
        Some(pane) => match beside(herdr, pane, &[(NOTE, &why)]) {
            Ok(how) => {
                return Ok(Opened::Beside {
                    pane: pane.into(),
                    why,
                    how,
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
fn beside_shepherd(herdr: &dyn Herdr, procs: &dyn Procs, place: &Place) -> Result<How> {
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
    beside(herdr, &place.pane, &[])
}

/// Focuses a view already in `pane`'s tab, else splits one right of it,
/// focused when the human isn't looking at that tab.
fn beside(herdr: &dyn Herdr, pane: &str, env: &[(&str, &str)]) -> Result<How> {
    let target = herdr.pane(pane)?;
    // A shell pane can carry the view's name; herdr refuses to plugin-focus it.
    if let Some(view) = views_in_tab_of(herdr, &target)
        .into_iter()
        .find(|view| herdr.focus_plugin_pane(view).is_ok())
    {
        return Ok(How::Focused { view });
    }
    // Unknown counts as seen: focus moves only when the human surely can't see the tab.
    let off_screen = herdr
        .visible_tab()
        .ok()
        .flatten()
        .is_some_and(|tab| tab != target.tab_id);
    herdr.open_plugin_pane(VIEW, &right_of(pane, off_screen), env)?;
    Ok(match off_screen {
        true => How::OpenedFocused,
        false => How::Opened,
    })
}

fn views_in_tab_of(herdr: &dyn Herdr, target: &Pane) -> Vec<String> {
    let panes = herdr.panes_in(&target.workspace_id).unwrap_or_default();
    panes
        .into_iter()
        .filter(|p| p.tab_id == target.tab_id && p.label.as_deref() == Some(VIEW_TITLE))
        .map(|p| p.id)
        .collect()
}

fn right_of(pane: &str, focus: bool) -> Placement {
    Placement::RightOf {
        pane: pane.into(),
        share: SHARE,
        focus,
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
        let Opened::Beside { pane, why, .. } = opened else {
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
                pane: shepherd.clone(),
                how: How::Opened
            }
        );
        assert_eq!(
            herdr.plugin_panes(),
            [(VIEW.to_string(), right_of(&shepherd, false), vec![])]
        );
    }

    #[test]
    fn a_second_open_beside_the_shepherd_focuses_its_view_instead_of_stacking() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Focused { view: view.clone() }
            }
        );
        assert_eq!(herdr.plugin_panes().len(), 1);
        assert_eq!(herdr.focused(), [view]);
    }

    #[test]
    fn run_from_its_own_view_the_action_focuses_that_view() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&view)).unwrap();

        let Opened::Beside { how, .. } = opened else {
            panic!("{opened:?}");
        };
        assert_eq!(how, How::Focused { view });
        assert_eq!(herdr.plugin_panes().len(), 1);
    }

    #[test]
    fn a_view_in_another_tab_doesnt_count_as_beside_the_shepherd() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let other = herdr.add_tab("w1", "other", Path::new("/src"), true);
        open(&herdr, &FakeProcs::pane(), state.path(), Some(&other)).unwrap();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Opened
            }
        );
        assert_eq!(herdr.plugin_panes().len(), 2);
    }

    #[test]
    fn a_shepherd_off_screen_gets_its_view_opened_with_focus_so_the_human_sees_it() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let elsewhere = herdr.add_tab("w1", "elsewhere", Path::new("/src"), true);
        herdr.show_tab_of(&elsewhere);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::OpenedFocused
            }
        );
        assert_eq!(herdr.plugin_panes()[0].1, right_of(&shepherd, true));
    }

    #[test]
    fn the_view_title_is_the_manifests() {
        let manifest: toml::Table = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin/herdr-plugin.toml"),
        )
        .unwrap()
        .parse()
        .unwrap();
        let panes = manifest["panes"].as_array().unwrap();
        let view = panes
            .iter()
            .find(|p| p["id"].as_str() == Some(VIEW))
            .unwrap();
        assert_eq!(view["title"].as_str(), Some(VIEW_TITLE));
    }

    #[test]
    fn a_shell_pane_named_like_the_view_doesnt_hide_the_open_view() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let decoy = herdr.split("w1:t1", Path::new("/src"));
        herdr.set_label(&decoy, VIEW_TITLE);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Focused { view: view.clone() }
            }
        );
        assert_eq!(herdr.plugin_panes().len(), 1);
        assert_eq!(herdr.focused(), [view]);
    }

    #[test]
    fn a_shepherd_on_screen_gets_its_view_opened_leaving_focus_put() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        herdr.show_tab_of(&shepherd);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), None).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::Opened
            }
        );
        assert_eq!(herdr.plugin_panes()[0].1, right_of(&shepherd, false));
        assert!(herdr.focused().is_empty());
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

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Opened
            }
        );
    }

    #[test]
    fn with_no_shepherd_recorded_the_view_opens_right_of_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();

        let opened = open(&herdr, &FakeProcs::pane(), state.path(), Some(&current)).unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert_eq!(herdr.plugin_panes()[0].1, right_of(&current, false));
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
