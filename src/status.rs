//! `loopherd status`: ledger members grouped by herd with herdr's live status.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::herd_view::{Herd, Member, Options, View, view};
use crate::herdr::{Herdr, MalformedReply};
use crate::ledger::{AgentRecord, Ledger};
use crate::pause;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `color` greys gone members.
    Text {
        color: bool,
    },
    Json,
}

#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// One read of herdr and the ledger, shared by `status` and the TUI.
#[derive(Debug)]
pub struct Read {
    pub view: View,
    /// Why every status is unknown, when it is, and pauses that couldn't be read; one per line.
    pub problem: Option<String>,
    /// 2 when herdr is unreachable, 3 when its `agent list` reply is malformed.
    pub code: i32,
}

/// Reads herdr once and the ledger once; changes neither. When herdr's agents
/// can't be read it still returns the ledger, every status unknown.
pub fn read(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    pauses: &pause::Pauses,
    options: Options,
) -> Result<Read> {
    read_with(herdr, ledger, Some(pauses), options, branch)
}

/// [`read`] without each member's branch, which costs a git call per member,
/// or the herds' pauses.
pub fn read_statuses(herdr: &dyn Herdr, ledger: &dyn Ledger, options: Options) -> Result<Read> {
    read_with(herdr, ledger, None, options, |_| None)
}

fn read_with(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    pauses: Option<&pause::Pauses>,
    options: Options,
    branch: impl Fn(&AgentRecord) -> Option<String>,
) -> Result<Read> {
    let entries = ledger.entries()?;
    let (agents, malformed, problem, code) = match herdr.agents() {
        Ok(agents) => (Some(agents), false, None, 0),
        Err(e) if e.downcast_ref::<MalformedReply>().is_some() => (
            None,
            true,
            Some(format!(
                "error: herdr's `agent list` output is malformed, every status is unknown: {e:#}"
            )),
            3,
        ),
        Err(e) => (
            None,
            false,
            Some(format!(
                "warning: herdr unreachable, every status is unknown: {e:#}"
            )),
            2,
        ),
    };
    let mut view = view(&entries, agents.as_deref(), options, branch);
    view.herdr_reachable = agents.is_some() || malformed;
    let mut problems: Vec<String> = problem.into_iter().collect();
    if let Some(pauses) = pauses {
        let in_ledger = entries.values().filter_map(|e| e.herd.clone());
        let (readings, warning) = pauses.of(in_ledger);
        problems.extend(warning);
        problems.extend(add_pauses(&mut view.herds, readings));
    }
    Ok(Read {
        view,
        problem: (!problems.is_empty()).then(|| problems.join("\n")),
        code,
    })
}

/// Marks each paused herd, adding one the view left out for having no
/// active members; returns a warning per pause it couldn't read.
fn add_pauses(herds: &mut Vec<Herd>, pauses: Vec<(String, pause::Reading)>) -> Vec<String> {
    let mut warnings = vec![];
    for (name, reading) in pauses {
        let Some(pause) = reading.pause() else {
            continue;
        };
        warnings.extend(reading.warning(&name));
        match herds.binary_search_by(|h| h.name.as_str().cmp(&name)) {
            Ok(i) => herds[i].paused = Some(pause),
            Err(i) => herds.insert(
                i,
                Herd {
                    name,
                    paused: Some(pause),
                    members: vec![],
                },
            ),
        }
    }
    warnings
}

/// Prints [`read`]'s view, exiting 2 if herdr is unreachable and 3 if its
/// `agent list` reply is malformed.
pub fn status(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    pauses: &pause::Pauses,
    options: Options,
    format: Format,
) -> Result<Output> {
    let Read {
        view,
        problem,
        code,
    } = read(herdr, ledger, pauses, options)?;
    let stdout = match format {
        Format::Text { color } => text(&view, color),
        Format::Json => serde_json::to_string_pretty(&view)? + "\n",
    };
    Ok(Output {
        stdout,
        stderr: problem.map(|p| p + "\n").unwrap_or_default(),
        code,
    })
}

/// The worktree's checked-out branch, `detached`, or `None` when there's no
/// worktree or git can't say. The repo's own branch says nothing about the agent's.
fn branch(record: &AgentRecord) -> Option<String> {
    branch_at(record.worktree.as_deref()?)
}

fn branch_at(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        // These override -C, so every row would show the same branch.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        // symbolic-ref --quiet exits 1 only for a detached HEAD.
        Some(1) => Some("detached".into()),
        _ => None,
    }
}

/// Member sections in display order, each with its header.
pub fn sections(view: &View) -> Vec<(String, &[Member])> {
    // Only --all shows a herd with no active members; its entries are under `retired`.
    let mut sections: Vec<(String, &[Member])> = view
        .herds
        .iter()
        .map(|h| {
            let mut header = h.name.clone();
            if h.members.is_empty() && h.paused.is_none() {
                header += " (retired only)";
            }
            if let Some(pause) = &h.paused {
                header += &format!("  {pause}");
            }
            (header, h.members.as_slice())
        })
        .collect();
    if !view.ungrouped.is_empty() {
        sections.push(("ungrouped".into(), &view.ungrouped));
    }
    if let Some(retired) = &view.retired {
        sections.push(("retired".into(), retired));
    }
    sections
}

fn text(view: &View, color: bool) -> String {
    let sections = sections(view);
    let rows: Vec<Vec<Vec<String>>> = sections
        .iter()
        .map(|(_, members)| members.iter().map(row).collect())
        .collect();
    let widths = widths(rows.iter().flatten());
    let mut out = String::new();
    for ((name, members), rows) in sections.iter().zip(&rows) {
        out += &format!("{name}\n");
        for (member, row) in members.iter().zip(rows) {
            let line = aligned(row, &widths);
            out += &match color && member.gone {
                true => format!("  \x1b[2m{line}\x1b[0m\n"),
                false => format!("  {line}\n"),
            };
        }
    }
    out += &stray_lines(view);
    if sections.iter().any(|(_, m)| m.iter().any(|m| m.gone)) {
        out += "gone: no live agent matches it; `loopherd revive` can bring it back\n";
    }
    out
}

pub fn row(member: &Member) -> Vec<String> {
    let status = match member.gone {
        true => "gone".into(),
        false => member.status.as_str().into(),
    };
    let place = match &member.tab {
        Some(tab) => format!("{tab}/{}", member.pane),
        None => member.pane.clone(),
    };
    let name = match member.lead {
        true => format!("{} (lead)", member.name),
        false => member.name.clone(),
    };
    vec![
        name,
        status,
        member.branch.clone().unwrap_or_else(|| "-".into()),
        member.worktree.clone().unwrap_or_else(|| "-".into()),
        place,
    ]
}

pub fn stray_lines(view: &View) -> String {
    let Some(count) = view.strays.count else {
        return match view.herdr_reachable {
            true => "strays unknown (herdr's agent list unreadable)\n".into(),
            false => "strays unknown (herdr unreachable)\n".into(),
        };
    };
    let noun = if count == 1 { "stray" } else { "strays" };
    let mut out = format!("+{count} {noun} (live in herdr, not in loopherd's ledger)\n");
    if let Some(strays) = &view.strays.agents {
        let rows: Vec<Vec<String>> = strays
            .iter()
            .map(|s| {
                vec![
                    s.pane.clone(),
                    s.cwd
                        .as_ref()
                        .map_or("-".into(), |c| c.display().to_string()),
                    s.title.clone().unwrap_or_default(),
                ]
            })
            .collect();
        let widths = widths(&rows);
        for row in &rows {
            out += &format!("  {}\n", aligned(row, &widths));
        }
    }
    out
}

pub fn widths<'a>(rows: impl IntoIterator<Item = &'a Vec<String>>) -> Vec<usize> {
    let mut widths = vec![];
    for row in rows {
        widths.resize(widths.len().max(row.len()), 0);
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    widths
}

pub fn aligned(row: &[String], widths: &[usize]) -> String {
    let cells: Vec<String> = row
        .iter()
        .zip(widths)
        .map(|(cell, w)| format!("{cell:w$}"))
        .collect();
    cells.join("  ").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::{Agent, AgentStatus};
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::pause::Pauses;

    fn entry(name: &str, herd: Option<&str>, worktree: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: format!("w1:p{name}"),
            session_id: Some(format!("sess-{name}")),
            repo: "/nonexistent/repo".into(),
            worktree: Some(format!("/nonexistent/{worktree}").into()),
            branch: None,
            workspace: "w1".into(),
            workspace_label: Some("main".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: herd.map(str::to_string),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
        }
    }

    fn agent(name: Option<&str>, pane: &str, status: AgentStatus) -> Agent {
        Agent {
            name: name.map(str::to_string),
            pane_id: pane.into(),
            tab_id: pane.replace(":p", ":t"),
            status,
            session_id: name.map(|n| format!("sess-{n}")),
            launch_pending: false,
            cwd: Some("/src/elsewhere".into()),
            title: Some("poking around".into()),
        }
    }

    fn ledger() -> MemLedger {
        let mut ledger = MemLedger::default();
        for e in [
            entry("zpr-1", Some("prs"), "pr-1"),
            entry("zpr-2", Some("prs"), "pr-2"),
            entry("lh-42", None, "feat-x"),
            entry("old", None, "old-wt"),
        ] {
            ledger.0.insert(e.name.clone(), e);
        }
        ledger
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "main")])
            .with_raw_agent(agent(Some("zpr-1"), "w1:pzpr-1", AgentStatus::Working))
            .with_raw_agent(agent(Some("zpr-2"), "w1:pzpr-2", AgentStatus::Blocked))
            .with_raw_agent(agent(Some("lh-42"), "w1:plh-42", AgentStatus::Idle))
            .with_raw_agent(agent(None, "w2:p7", AgentStatus::Idle))
    }

    const PLAIN: Format = Format::Text { color: false };
    fn not_paused() -> Pauses {
        Pauses::under(Path::new("/nonexistent"))
    }

    /// A config root with a `paused` dir and an empty `herds` dir.
    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("paused")).unwrap();
        std::fs::create_dir(dir.path().join("herds")).unwrap();
        dir
    }

    /// A config root pausing `prs` since 09:00, with `reason`.
    fn paused_prs(reason: Option<&str>) -> tempfile::TempDir {
        let dir = root();
        let pause = serde_json::json!({"since": "2026-11-05T09:00:00Z", "reason": reason});
        std::fs::write(dir.path().join("paused/prs"), pause.to_string()).unwrap();
        dir
    }

    #[test]
    fn a_paused_herds_header_says_since_when_and_why() {
        let herdr = herdr();
        let ledger = ledger();
        for (reason, header) in [
            (
                Some("freeze"),
                "prs  paused since 2026-11-05T09:00:00Z — freeze\n",
            ),
            (None, "prs  paused since 2026-11-05T09:00:00Z\n"),
        ] {
            let paused = paused_prs(reason);
            let out = status(
                &herdr,
                &ledger,
                &Pauses::under(paused.path()),
                Options::default(),
                PLAIN,
            )
            .unwrap();
            assert!(out.stdout.starts_with(header), "{}", out.stdout);
            assert!(out.stdout.contains("\nungrouped\n"), "{}", out.stdout);
        }
    }

    #[test]
    fn a_paused_herd_shows_with_only_retired_members_or_only_a_herd_file() {
        let mut ledger = ledger();
        for name in ["zpr-1", "zpr-2"] {
            ledger.0.get_mut(name).unwrap().retired = true;
        }
        let root = paused_prs(None);
        let docs = r#"{"since": "2026-11-05T10:00:00Z", "reason": null}"#;
        std::fs::write(root.path().join("paused/docs"), docs).unwrap();
        std::fs::write(root.path().join("herds/docs.toml"), "repo = \"/r\"\n").unwrap();
        let paths = Pauses::under(root.path());

        let out = status(&herdr(), &ledger, &paths, Options::default(), PLAIN).unwrap();

        assert!(
            out.stdout.starts_with(
                "docs  paused since 2026-11-05T10:00:00Z\n\
                 prs  paused since 2026-11-05T09:00:00Z\n\
                 ungrouped\n"
            ),
            "{}",
            out.stdout
        );
        let json = status(&herdr(), &ledger, &paths, Options::default(), Format::Json);
        let json: serde_json::Value = serde_json::from_str(&json.unwrap().stdout).unwrap();
        assert_eq!(json["herds"][1]["name"], "prs");
        assert_eq!(json["herds"][1]["paused"]["since"], "2026-11-05T09:00:00Z");
        assert_eq!(json["herds"][1]["members"], serde_json::json!([]));
    }

    #[test]
    fn a_pause_for_a_herd_with_no_herd_file_and_no_members_is_not_shown() {
        let root = root();
        let gone = r#"{"since": "2026-11-05T10:00:00Z", "reason": null}"#;
        std::fs::write(root.path().join("paused/gone"), gone).unwrap();

        let out = status(
            &herdr(),
            &ledger(),
            &Pauses::under(root.path()),
            Options::default(),
            PLAIN,
        )
        .unwrap();

        assert!(!out.stdout.contains("gone  paused"), "{}", out.stdout);
        assert_eq!(out.stderr, "");
    }

    #[test]
    fn an_unreadable_pause_warns_and_shows_the_herd_paused_with_its_time_unknown() {
        let root = root();
        std::fs::write(root.path().join("paused/prs"), "").unwrap();
        let paths = Pauses::under(root.path());

        let out = status(&herdr(), &ledger(), &paths, Options::default(), PLAIN).unwrap();

        assert!(
            out.stdout.starts_with("prs  paused (time unknown)\n"),
            "{}",
            out.stdout
        );
        assert!(
            out.stderr
                .starts_with("warning: prs's pause is unreadable, counted as paused: "),
            "{}",
            out.stderr
        );
        assert_eq!(out.code, 0);
        let json = status(
            &herdr(),
            &ledger(),
            &paths,
            Options::default(),
            Format::Json,
        );
        let json: serde_json::Value = serde_json::from_str(&json.unwrap().stdout).unwrap();
        assert_eq!(
            json["herds"][0]["paused"],
            serde_json::json!({"since": null, "reason": null})
        );
    }

    /// `status`'s text for a config root, after `prepare` mangles its paused dir.
    fn status_with_paused_dir(prepare: impl Fn(&Path)) -> Output {
        use std::os::unix::fs::PermissionsExt;
        let root = paused_prs(None);
        let paused = root.path().join("paused");
        prepare(&paused);
        let out = status(
            &herdr(),
            &ledger(),
            &Pauses::under(root.path()),
            Options::default(),
            PLAIN,
        );
        std::fs::set_permissions(&paused, std::fs::Permissions::from_mode(0o755)).ok();
        out.unwrap()
    }

    #[test]
    fn a_paused_dir_that_cannot_be_read_fails_closed_for_each_known_herd() {
        use std::os::unix::fs::PermissionsExt;
        let file_in_its_place = |dir: &Path| {
            std::fs::remove_dir_all(dir).unwrap();
            std::fs::write(dir, "").unwrap();
        };
        let mode_000 = |dir: &Path| {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap()
        };
        for prepare in [&file_in_its_place as &dyn Fn(&Path), &mode_000] {
            let out = status_with_paused_dir(prepare);
            assert!(
                out.stdout.starts_with("prs  paused (time unknown)\n"),
                "{}",
                out.stdout
            );
            assert!(
                out.stderr
                    .starts_with("warning: prs's pause is unreadable, counted as paused: "),
                "{}",
                out.stderr
            );
        }
    }

    #[test]
    fn a_paused_dir_that_cannot_be_listed_still_shows_each_known_herds_pause() {
        use std::os::unix::fs::PermissionsExt;
        let out = status_with_paused_dir(|dir| {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o300)).unwrap()
        });
        assert!(
            out.stdout
                .starts_with("prs  paused since 2026-11-05T09:00:00Z\n"),
            "{}",
            out.stdout
        );
        assert_eq!(out.stderr, "");
    }

    /// A config root pausing `docs`, which has a herd file and no ledger members.
    fn paused_docs() -> tempfile::TempDir {
        let root = root();
        let docs = r#"{"since": "2026-11-05T10:00:00Z", "reason": null}"#;
        std::fs::write(root.path().join("paused/docs"), docs).unwrap();
        std::fs::write(root.path().join("herds/docs.toml"), "repo = \"/r\"\n").unwrap();
        root
    }

    #[test]
    fn herd_files_that_cannot_be_listed_warn_that_memberless_pauses_are_hidden() {
        use std::os::unix::fs::PermissionsExt;
        let root = paused_docs();
        let herds = root.path().join("herds");
        std::fs::set_permissions(&herds, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = status(
            &herdr(),
            &ledger(),
            &Pauses::under(root.path()),
            Options::default(),
            PLAIN,
        );
        std::fs::set_permissions(&herds, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = out.unwrap();
        assert!(!out.stdout.contains("docs  paused"), "{}", out.stdout);
        assert!(
            out.stderr.starts_with(
                "warning: can't list herd files, so a paused herd with no members isn't shown: read "
            ),
            "{}",
            out.stderr
        );
    }

    #[test]
    fn no_config_dir_warns_that_memberless_pauses_are_hidden() {
        let root = paused_docs();
        let pauses = Pauses {
            dir: root.path().join("paused"),
            herds: None,
        };

        let out = status(&herdr(), &ledger(), &pauses, Options::default(), PLAIN).unwrap();

        assert!(!out.stdout.contains("docs  paused"), "{}", out.stdout);
        assert_eq!(
            out.stderr,
            "warning: no config dir (set HOME or XDG_CONFIG_HOME), \
             so a paused herd with no members isn't shown\n"
        );
    }

    #[test]
    fn a_hand_edited_reason_cannot_split_the_header() {
        let root = root();
        let pause = r#"{"since": "2026-11-05T09:00:00Z", "reason": "a\nungrouped\u001b[2J"}"#;
        std::fs::write(root.path().join("paused/prs"), pause).unwrap();

        let out = status(
            &herdr(),
            &ledger(),
            &Pauses::under(root.path()),
            Options::default(),
            PLAIN,
        )
        .unwrap();

        assert!(
            out.stdout.starts_with(
                "prs  paused since 2026-11-05T09:00:00Z — a\\u{a}ungrouped\\u{1b}[2J\n"
            ),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn json_gives_each_herd_its_pause_or_null() {
        let ledger = ledger();
        let paused = paused_prs(Some("freeze"));
        let json = |paths: &Pauses| {
            let out = status(&herdr(), &ledger, paths, Options::default(), Format::Json).unwrap();
            serde_json::from_str::<serde_json::Value>(&out.stdout).unwrap()["herds"][0]["paused"]
                .clone()
        };
        assert_eq!(
            json(&Pauses::under(paused.path())),
            serde_json::json!({"since": "2026-11-05T09:00:00Z", "reason": "freeze"})
        );
        assert_eq!(json(&not_paused()), serde_json::Value::Null);
    }

    #[test]
    fn prints_members_by_herd_then_ungrouped_with_gone_hinted_and_strays_counted() {
        let herdr = herdr();
        let ledger = ledger();
        let before = ledger.0.clone();
        let out = status(&herdr, &ledger, &not_paused(), Options::default(), PLAIN).unwrap();
        assert_eq!(
            out.stdout,
            "\
prs
  zpr-2  blocked  -  pr-2    w1:tzpr-2/w1:pzpr-2
  zpr-1  working  -  pr-1    w1:tzpr-1/w1:pzpr-1
ungrouped
  lh-42  idle     -  feat-x  w1:tlh-42/w1:plh-42
  old    gone     -  old-wt  w1:pold
+1 stray (live in herdr, not in loopherd's ledger)
gone: no live agent matches it; `loopherd revive` can bring it back
"
        );
        assert_eq!(out.stderr, "");
        assert_eq!(out.code, 0);
        assert_eq!(herdr.mutations(), 0);
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn a_herds_lead_comes_first_in_its_herd_marked_as_the_lead() {
        let herdr = herdr().with_raw_agent(agent(
            Some("lead-prs"),
            "w1:plead-prs",
            AgentStatus::Working,
        ));
        let mut ledger = ledger();
        let mut lead = entry("lead-prs", Some("prs"), "lead-prs");
        lead.role = Some(crate::ledger::Role::Lead);
        ledger.0.insert(lead.name.clone(), lead);

        let out = status(&herdr, &ledger, &not_paused(), Options::default(), PLAIN).unwrap();

        assert!(
            out.stdout.starts_with(
                "\
prs
  lead-prs (lead)  working  -  lead-prs  w1:tlead-prs/w1:plead-prs
  zpr-2            blocked  -  pr-2      w1:tzpr-2/w1:pzpr-2
"
            ),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn with_herdr_unreachable_prints_the_ledger_as_unknown_warns_and_exits_2() {
        let mut herdr = herdr();
        herdr.unreachable = true;
        let out = status(&herdr, &ledger(), &not_paused(), Options::default(), PLAIN).unwrap();
        assert_eq!(
            out.stdout,
            "\
prs
  zpr-1  unknown  -  pr-1    w1:pzpr-1
  zpr-2  unknown  -  pr-2    w1:pzpr-2
ungrouped
  lh-42  unknown  -  feat-x  w1:plh-42
  old    unknown  -  old-wt  w1:pold
strays unknown (herdr unreachable)
"
        );
        assert_eq!(
            out.stderr,
            "warning: herdr unreachable, every status is unknown: herdr server not running\n"
        );
        assert_eq!(out.code, 2);
    }

    #[test]
    fn all_and_strays_add_retired_and_stray_sections_and_color_greys_gone_rows() {
        let mut ledger = ledger();
        ledger.0.get_mut("zpr-1").unwrap().retired = true;
        let out = status(
            &herdr(),
            &ledger,
            &not_paused(),
            Options {
                all: true,
                strays: true,
            },
            Format::Text { color: true },
        )
        .unwrap();
        assert_eq!(
            out.stdout,
            "\
prs
  zpr-2  blocked  -  pr-2    w1:tzpr-2/w1:pzpr-2
ungrouped
  lh-42  idle     -  feat-x  w1:tlh-42/w1:plh-42
  \x1b[2mold    gone     -  old-wt  w1:pold\x1b[0m
retired
  zpr-1  working  -  pr-1    w1:tzpr-1/w1:pzpr-1
+1 stray (live in herdr, not in loopherd's ledger)
  w2:p7  /src/elsewhere  poking around
gone: no live agent matches it; `loopherd revive` can bring it back
"
        );
    }

    #[test]
    fn all_marks_a_herd_with_only_retired_entries_instead_of_a_bare_header() {
        let mut ledger = ledger();
        for name in ["zpr-1", "zpr-2"] {
            ledger.0.get_mut(name).unwrap().retired = true;
        }
        let all = Options {
            all: true,
            strays: false,
        };
        let out = status(&herdr(), &ledger, &not_paused(), all, PLAIN).unwrap();
        assert!(
            out.stdout.starts_with("prs (retired only)\nungrouped\n"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn json_has_the_documented_shape() {
        let mut ledger = ledger();
        ledger.0.get_mut("old").unwrap().retired = true;
        let out = status(
            &herdr(),
            &ledger,
            &not_paused(),
            Options {
                all: true,
                strays: true,
            },
            Format::Json,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        let member = |name: &str, status: &str, gone: bool, wt: &str, pane: &str, tab| {
            serde_json::json!({"name": name, "status": status, "gone": gone, "branch": null,
                "worktree": wt, "pane": pane, "tab": tab, "lead": false})
        };
        assert_eq!(
            json,
            serde_json::json!({
                "herdr_reachable": true,
                "herds": [{"name": "prs", "paused": null, "members": [
                    member("zpr-2", "blocked", false, "pr-2", "w1:pzpr-2", Some("w1:tzpr-2")),
                    member("zpr-1", "working", false, "pr-1", "w1:pzpr-1", Some("w1:tzpr-1")),
                ]}],
                "ungrouped": [member("lh-42", "idle", false, "feat-x", "w1:plh-42", Some("w1:tlh-42"))],
                "retired": [member("old", "unknown", false, "old-wt", "w1:pold", None)],
                "strays": {"count": 1, "agents": [{"pane": "w2:p7", "tab": "w2:t7", "status": "idle",
                    "cwd": "/src/elsewhere", "title": "poking around"}]},
            })
        );

        let default = status(
            &herdr(),
            &ledger,
            &not_paused(),
            Options::default(),
            Format::Json,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&default.stdout).unwrap();
        assert_eq!(json.get("retired"), None);
        assert_eq!(json["strays"], serde_json::json!({"count": 1}));
    }

    #[test]
    fn no_path_mutates_herdr_or_writes_the_ledger() {
        for unreachable in [false, true] {
            for (all, strays) in [(false, false), (true, true)] {
                for format in [PLAIN, Format::Text { color: true }, Format::Json] {
                    let mut herdr = herdr();
                    herdr.unreachable = unreachable;
                    let ledger = ledger();
                    let before = ledger.0.clone();
                    status(
                        &herdr,
                        &ledger,
                        &not_paused(),
                        Options { all, strays },
                        format,
                    )
                    .unwrap();
                    assert_eq!(herdr.mutations(), 0);
                    assert_eq!(ledger.0, before);
                }
            }
        }
    }

    #[test]
    fn shows_the_checked_out_branch_or_detached() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let mut on_branch = entry("a", None, "x");
        on_branch.worktree = Some(repo.clone());
        assert_eq!(branch(&on_branch).as_deref(), Some("main"));

        crate::worktree::testing::git_in(&repo, &["checkout", "--quiet", "--detach"]);
        assert_eq!(branch(&on_branch).as_deref(), Some("detached"));
        assert_eq!(branch(&entry("b", None, "missing")), None);
    }

    #[test]
    fn an_entry_without_a_worktree_shows_no_branch_or_worktree_not_the_repos() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let mut ledger = MemLedger::default();
        let mut e = entry("lh-1", None, "unused");
        e.repo = repo;
        e.worktree = None;
        ledger.0.insert(e.name.clone(), e);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_raw_agent(agent(
            Some("lh-1"),
            "w1:plh-1",
            AgentStatus::Idle,
        ));
        let out = status(&herdr, &ledger, &not_paused(), Options::default(), PLAIN).unwrap();
        assert_eq!(
            out.stdout.lines().nth(1),
            Some("  lh-1  idle  -  -  w1:tlh-1/w1:plh-1")
        );
        let json = status(
            &herdr,
            &ledger,
            &not_paused(),
            Options::default(),
            Format::Json,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        let member = &json["ungrouped"][0];
        assert_eq!(
            (&member["branch"], &member["worktree"]),
            (&serde_json::Value::Null, &serde_json::Value::Null)
        );
    }

    #[test]
    fn read_statuses_skips_the_branch_lookup() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = MemLedger::default();
        let mut e = entry("lh-1", None, "unused");
        e.worktree = Some(crate::worktree::testing::repo_with_origin(root.path()));
        ledger.0.insert(e.name.clone(), e);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_raw_agent(agent(
            Some("lh-1"),
            "w1:plh-1",
            AgentStatus::Working,
        ));
        let member = |read: Read| read.view.ungrouped[0].clone();

        let full = member(read(&herdr, &ledger, &not_paused(), Options::default()).unwrap());
        let statuses = member(read_statuses(&herdr, &ledger, Options::default()).unwrap());

        assert_eq!(full.branch.as_deref(), Some("main"));
        assert_eq!(statuses.branch, None);
        assert_eq!(statuses.status, AgentStatus::Working);
    }
}
