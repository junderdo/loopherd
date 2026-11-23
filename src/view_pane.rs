//! Opening the herd view in a pane beside the shepherd's, the one its
//! handoff record names, rather than in the plugin's popup, on the side
//! `view.layout` picks.

use std::path::Path;

use anyhow::{Result, bail};

use crate::herdr::{Herdr, Pane, Placement, Side};
use crate::ledger::Process;
use crate::process::{self, Procs};
use crate::shepherd_resume::{self, Place};

const VIEW: &str = "view";
/// The view pane's title in the plugin manifest, which herdr shows as its label.
const VIEW_TITLE: &str = "loopherd view";
/// Set on a view opened anywhere but beside the shepherd, saying why.
pub const NOTE: &str = "LOOPHERD_VIEW_NOTE";
/// The view's share of the split it makes or is made beside.
pub const SHARE: f64 = 0.4;

/// How the view and the shepherd share a tab, set as `view.layout`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Layout {
    /// The shepherd left, the view right.
    #[default]
    SideBySide,
    /// The view on top, the shepherd below.
    Stacked,
}

impl Layout {
    const ALL: [Layout; 2] = [Layout::SideBySide, Layout::Stacked];

    pub fn name(self) -> &'static str {
        match self {
            Layout::SideBySide => "side-by-side",
            Layout::Stacked => "stacked",
        }
    }

    /// Where the view goes relative to the shepherd's pane.
    fn view_side(self) -> Side {
        match self {
            Layout::SideBySide => Side::Right,
            Layout::Stacked => Side::Above,
        }
    }

    /// Where a shepherd goes relative to the view's pane.
    pub fn shepherd_side(self) -> Side {
        match self {
            Layout::SideBySide => Side::Left,
            Layout::Stacked => Side::Below,
        }
    }
}

impl std::str::FromStr for Layout {
    type Err = String;

    fn from_str(name: &str) -> Result<Layout, String> {
        Layout::ALL
            .into_iter()
            .find(|l| l.name() == name)
            .ok_or_else(|| {
                let names: Vec<_> = Layout::ALL.map(Layout::name).into();
                format!("unknown layout {name:?}: name one of {}", names.join(", "))
            })
    }
}

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

/// Opens the view beside the shepherd's recorded pane in `state`, else
/// beside `current`, the pane the action ran from, else in the popup;
/// `layout` says which side.
pub fn open(
    herdr: &dyn Herdr,
    procs: &dyn Procs,
    state: &Path,
    current: Option<&str>,
    layout: Layout,
) -> Result<Opened> {
    let side = layout.view_side();
    let why = match shepherd_resume::recorded_place_and_shell(state) {
        None => "no shepherd recorded yet; its first handoff records its pane".to_string(),
        Some((place, _)) if herdr.socket().as_ref() != Some(&place.socket) => format!(
            "the recorded shepherd ({}) is in another herdr session",
            place.pane
        ),
        Some((place, shell)) => match beside_shepherd(herdr, procs, &place, shell, side) {
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
        Some(pane) => match beside(herdr, pane, side, &[(NOTE, &why)]) {
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

/// A pane id outlives its pane, so the pane must still run the recorded
/// shell and an agent; a record without a shell needs the shepherd's Claude
/// to be the pane's shell or under it.
fn beside_shepherd(
    herdr: &dyn Herdr,
    procs: &dyn Procs,
    place: &Place,
    recorded_shell: Option<Process>,
    side: Side,
) -> Result<How> {
    let Some(shell) = herdr.shell_pid(&place.pane)? else {
        bail!("its pane has no shell yet, as while herdr resumes its agent");
    };
    let Some(start) = procs.start(shell) else {
        bail!("can't read its shell's process (pid {shell})");
    };
    match recorded_shell {
        Some(recorded) if recorded.pid != shell => bail!(
            "its shell changed (pid {shell}, was the shepherd's pid {})",
            recorded.pid
        ),
        Some(recorded) if recorded.start != start => {
            bail!("its shell was restarted (pid {shell} is a new process)")
        }
        // The shell outlives a Claude restarted in it, and a Claude quit in it.
        Some(_) => {
            if !herdr.agents()?.iter().any(|a| a.pane_id == place.pane) {
                bail!("its pane runs no agent now, only the shepherd's shell");
            }
        }
        None => {
            let under_shell = process::live(procs, Some(place.process)).is_some_and(|claude| {
                claude.pid == shell || process::is_ancestor(procs, shell, claude.pid)
            });
            if !under_shell {
                bail!(
                    "it no longer runs the shepherd's Claude (pid {})",
                    place.process.pid
                );
            }
        }
    }
    beside(herdr, &place.pane, side, &[])
}

/// Focuses a view already in `pane`'s tab, else splits one off it on
/// `side`, focused when the human isn't looking at that tab.
fn beside(herdr: &dyn Herdr, pane: &str, side: Side, env: &[(&str, &str)]) -> Result<How> {
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
    let placement = Placement::Split {
        pane: pane.into(),
        side,
        share: SHARE,
        focus: off_screen,
    };
    herdr.open_plugin_pane(VIEW, &placement, env)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::process::{CLAUDE, FakeProcs, PANE_SHELL};
    use std::path::PathBuf;
    use std::time::SystemTime;

    /// A herdr whose one pane, `w1:p1`, runs [`FakeProcs::pane`]'s shell and
    /// an agent: the shepherd's Claude, pid 30.
    fn herdr_with_tab() -> (FakeHerdr, String) {
        bare_herdr_with_tab(
            FakeHerdr::new(&[("w1", "loopherd")]).with_live_agent(None, None, "w1:p1"),
        )
    }

    /// `herdr`, given a tab whose pane `w1:p1` runs [`FakeProcs::pane`]'s shell.
    fn bare_herdr_with_tab(herdr: FakeHerdr) -> (FakeHerdr, String) {
        let pane = herdr
            .create_tab("w1", Path::new("/src"), "shepherd")
            .unwrap()
            .pane_id;
        assert_eq!(pane, "w1:p1");
        herdr.set_shell_pid(&pane, Some(PANE_SHELL.pid));
        (herdr, pane)
    }

    fn record_shepherd(state: &Path, pane: &str, socket: &str) {
        record_shepherd_with(state, pane, socket, Some(PANE_SHELL));
    }

    /// A record from before the pane's shell was recorded has `shell` `None`.
    fn record_shepherd_with(state: &Path, pane: &str, socket: &str, shell: Option<Process>) {
        let place = shepherd_resume::Place {
            pane: pane.into(),
            socket: PathBuf::from(socket),
            process: Process {
                pid: 30,
                start: 300,
            },
        };
        shepherd_resume::record(state, &place, shell, "s-1", SystemTime::now()).unwrap();
    }

    fn note(herdr: &FakeHerdr) -> String {
        let [(_, _, env)] = herdr.plugin_panes().try_into().unwrap();
        let [(name, why)] = env.try_into().unwrap();
        assert_eq!(name, NOTE);
        why
    }

    fn placed(pane: &str, side: Side, focus: bool) -> Placement {
        Placement::Split {
            pane: pane.into(),
            side,
            share: 0.4,
            focus,
        }
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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::Opened
            }
        );
        assert_eq!(
            herdr.plugin_panes(),
            [(
                VIEW.to_string(),
                placed(&shepherd, Side::Right, false),
                vec![]
            )]
        );
    }

    #[test]
    fn a_second_open_beside_the_shepherd_focuses_its_view_instead_of_stacking() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

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
        open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&view),
            Layout::SideBySide,
        )
        .unwrap();

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
        open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&other),
            Layout::SideBySide,
        )
        .unwrap();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::OpenedFocused
            }
        );
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&shepherd, Side::Right, true)
        );
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
        open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::Opened
            }
        );
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&shepherd, Side::Right, false)
        );
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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&current, Side::Right, false)
        );
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_shepherd_pane_gone_opens_right_of_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        record_shepherd(state.path(), "w1:p99", FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

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
        let mut procs = FakeProcs::pane();
        procs.add(21, "/bin/bash", 10, 210);

        let opened = open(
            &herdr,
            &procs,
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("its shell changed (pid 21"), "{why}");
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_shepherd_pane_restarted_with_herdr_counts_as_gone() {
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        for (shell, said) in [
            (
                Some(PANE_SHELL),
                "its shell was restarted (pid 20 is a new process)",
            ),
            (None, "no longer runs the shepherd's Claude"),
        ] {
            let state = tempfile::tempdir().unwrap();
            record_shepherd_with(state.path(), &shepherd, FAKE_SOCKET, shell);

            let restarted = FakeProcs::restarted_pane();
            let opened = open(
                &herdr,
                &restarted,
                state.path(),
                Some(&current),
                Layout::SideBySide,
            )
            .unwrap();

            let why = beside_current_why(opened, &current);
            assert!(why.contains(said), "{why}");
        }
    }

    #[test]
    fn a_shepherd_claude_restarted_in_its_pane_still_counts() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        let mut resumed = FakeProcs::default();
        resumed.add(10, "/usr/bin/herdr", 1, 100);
        resumed.add(20, "/bin/bash", 10, 200);

        let opened = open(&herdr, &resumed, state.path(), None, Layout::SideBySide).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Opened
            }
        );
    }

    #[test]
    fn an_agent_list_herdr_cant_give_opens_right_of_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (mut herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        herdr.malformed_agents = true;
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(
            why.contains("not beside the shepherd's pane w1:p1"),
            "{why}"
        );
        assert!(why.contains("agent list"), "{why}");
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_shepherd_shell_left_at_a_bare_prompt_counts_as_gone() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = bare_herdr_with_tab(FakeHerdr::new(&[("w1", "loopherd")]));
        let current = herdr.split("w1:t1", Path::new("/src"));
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        let mut quit = FakeProcs::default();
        quit.add(10, "/usr/bin/herdr", 1, 100);
        quit.add(20, "/bin/bash", 10, 200);

        let opened = open(
            &herdr,
            &quit,
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("runs no agent"), "{why}");
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn a_record_without_a_shell_counts_a_shepherd_claude_execed_as_the_panes_shell() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        herdr.set_shell_pid(&shepherd, Some(30));
        record_shepherd_with(state.path(), &shepherd, FAKE_SOCKET, None);
        let mut execed = FakeProcs::default();
        execed.add(10, "/usr/bin/herdr", 1, 100);
        execed.add(30, CLAUDE, 10, 300);

        let opened = open(&herdr, &execed, state.path(), None, Layout::SideBySide).unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd,
                how: How::Opened
            }
        );
    }

    #[test]
    fn a_shepherd_pane_without_a_shell_says_so() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        herdr.set_shell_pid(&shepherd, None);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("has no shell"), "{why}");
    }

    #[test]
    fn a_shepherd_pane_whose_processes_cant_be_read_says_so() {
        let (herdr, shepherd) = herdr_with_tab();
        let current = herdr.split("w1:t1", Path::new("/src"));
        for shell in [Some(PANE_SHELL), None] {
            let state = tempfile::tempdir().unwrap();
            record_shepherd_with(state.path(), &shepherd, FAKE_SOCKET, shell);

            let opened = open(
                &herdr,
                &FakeProcs::default(),
                state.path(),
                Some(&current),
                Layout::SideBySide,
            );

            let why = beside_current_why(opened.unwrap(), &current);
            assert!(why.contains("can't read"), "{why}");
        }
    }

    #[test]
    fn a_shepherd_in_another_herdr_session_counts_as_not_here() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        record_shepherd(state.path(), &current, "/run/other/herdr.sock");

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::SideBySide,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("another herdr session"), "{why}");
    }

    #[test]
    fn with_no_shepherd_and_no_current_pane_the_view_opens_in_the_popup_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::SideBySide,
        )
        .unwrap();

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

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some("w1:p77"),
            Layout::SideBySide,
        )
        .unwrap();

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

    #[test]
    fn stacked_the_view_opens_above_the_recorded_shepherd_pane_leaving_focus_put() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        herdr.show_tab_of(&shepherd);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::Opened
            }
        );
        assert_eq!(
            herdr.plugin_panes(),
            [(
                VIEW.to_string(),
                placed(&shepherd, Side::Above, false),
                vec![]
            )]
        );
        assert!(herdr.focused().is_empty());
    }

    #[test]
    fn stacked_a_shepherd_off_screen_gets_its_view_opened_above_it_with_focus() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        let elsewhere = herdr.add_tab("w1", "elsewhere", Path::new("/src"), true);
        herdr.show_tab_of(&elsewhere);
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::Stacked,
        )
        .unwrap();

        assert_eq!(
            opened,
            Opened::BesideShepherd {
                pane: shepherd.clone(),
                how: How::OpenedFocused
            }
        );
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&shepherd, Side::Above, true)
        );
    }

    #[test]
    fn stacked_a_second_open_focuses_the_view_already_above_the_shepherd() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, shepherd) = herdr_with_tab();
        record_shepherd(state.path(), &shepherd, FAKE_SOCKET);
        open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::Stacked,
        )
        .unwrap();
        let view = herdr.plugin_pane_ids()[0].clone();

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::Stacked,
        )
        .unwrap();

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
    fn stacked_with_no_shepherd_recorded_the_view_opens_above_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::Stacked,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("no shepherd recorded"), "{why}");
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&current, Side::Above, false)
        );
        assert_eq!(note(&herdr), why);
    }

    #[test]
    fn stacked_a_shepherd_pane_gone_opens_above_the_current_pane_saying_why() {
        let state = tempfile::tempdir().unwrap();
        let (herdr, current) = herdr_with_tab();
        record_shepherd(state.path(), "w1:p99", FAKE_SOCKET);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            Some(&current),
            Layout::Stacked,
        )
        .unwrap();

        let why = beside_current_why(opened, &current);
        assert!(why.contains("w1:p99 not found"), "{why}");
        assert_eq!(
            herdr.plugin_panes()[0].1,
            placed(&current, Side::Above, false)
        );
    }

    #[test]
    fn stacked_with_no_shepherd_and_no_current_pane_the_view_opens_in_the_popup() {
        let state = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);

        let opened = open(
            &herdr,
            &FakeProcs::pane(),
            state.path(),
            None,
            Layout::Stacked,
        )
        .unwrap();

        let Opened::Popup { why } = opened else {
            panic!("{opened:?}");
        };
        assert_eq!(
            herdr.popups(),
            [(VIEW.to_string(), vec![(NOTE.to_string(), why)])]
        );
    }
}
