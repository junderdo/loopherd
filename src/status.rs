//! `loopherd status`: ledger members grouped by herd with herdr's live status.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::herd_view::{Member, Options, View, view};
use crate::herdr::{Herdr, MalformedReply};
use crate::ledger::{AgentRecord, Ledger};

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

/// Reads herdr once and the ledger once; changes neither. When herdr's agents
/// can't be read it still prints the ledger, exiting 2 if herdr is unreachable
/// and 3 if its `agent list` reply is malformed.
pub fn status(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    options: Options,
    format: Format,
) -> Result<Output> {
    let entries = ledger.entries()?;
    let (agents, malformed, stderr, code) = match herdr.agents() {
        Ok(agents) => (Some(agents), false, String::new(), 0),
        Err(e) if e.downcast_ref::<MalformedReply>().is_some() => (
            None,
            true,
            format!(
                "error: herdr's `agent list` output is malformed, every status is unknown: {e:#}\n"
            ),
            3,
        ),
        Err(e) => (
            None,
            false,
            format!("warning: herdr unreachable, every status is unknown: {e:#}\n"),
            2,
        ),
    };
    let mut view = view(&entries, agents.as_deref(), options, branch);
    view.herdr_reachable = agents.is_some() || malformed;
    let stdout = match format {
        Format::Text { color } => text(&view, color),
        Format::Json => serde_json::to_string_pretty(&view)? + "\n",
    };
    Ok(Output {
        stdout,
        stderr,
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

fn text(view: &View, color: bool) -> String {
    // Only --all shows a herd with no active members; its entries are under `retired`.
    let mut sections: Vec<(String, &[Member])> = view
        .herds
        .iter()
        .map(|h| match h.members.is_empty() {
            true => (format!("{} (retired only)", h.name), h.members.as_slice()),
            false => (h.name.clone(), h.members.as_slice()),
        })
        .collect();
    if !view.ungrouped.is_empty() {
        sections.push(("ungrouped".into(), &view.ungrouped));
    }
    if let Some(retired) = &view.retired {
        sections.push(("retired".into(), retired));
    }
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

fn row(member: &Member) -> Vec<String> {
    let status = match member.gone {
        true => "gone".into(),
        false => member.status.as_str().into(),
    };
    let place = match &member.tab {
        Some(tab) => format!("{tab}/{}", member.pane),
        None => member.pane.clone(),
    };
    vec![
        member.name.clone(),
        status,
        member.branch.clone().unwrap_or_else(|| "-".into()),
        member.worktree.clone().unwrap_or_else(|| "-".into()),
        place,
    ]
}

fn stray_lines(view: &View) -> String {
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

fn widths<'a>(rows: impl IntoIterator<Item = &'a Vec<String>>) -> Vec<usize> {
    let mut widths = vec![];
    for row in rows {
        widths.resize(widths.len().max(row.len()), 0);
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    widths
}

fn aligned(row: &[String], widths: &[usize]) -> String {
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

    fn entry(name: &str, herd: Option<&str>, worktree: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: format!("w1:p{name}"),
            session_id: Some(format!("sess-{name}")),
            repo: "/nonexistent/repo".into(),
            worktree: Some(format!("/nonexistent/{worktree}").into()),
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

    #[test]
    fn prints_members_by_herd_then_ungrouped_with_gone_hinted_and_strays_counted() {
        let herdr = herdr();
        let ledger = ledger();
        let before = ledger.0.clone();
        let out = status(&herdr, &ledger, Options::default(), PLAIN).unwrap();
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
    fn with_herdr_unreachable_prints_the_ledger_as_unknown_warns_and_exits_2() {
        let mut herdr = herdr();
        herdr.unreachable = true;
        let out = status(&herdr, &ledger(), Options::default(), PLAIN).unwrap();
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
        let out = status(&herdr(), &ledger, all, PLAIN).unwrap();
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
                "worktree": wt, "pane": pane, "tab": tab})
        };
        assert_eq!(
            json,
            serde_json::json!({
                "herdr_reachable": true,
                "herds": [{"name": "prs", "members": [
                    member("zpr-2", "blocked", false, "pr-2", "w1:pzpr-2", Some("w1:tzpr-2")),
                    member("zpr-1", "working", false, "pr-1", "w1:pzpr-1", Some("w1:tzpr-1")),
                ]}],
                "ungrouped": [member("lh-42", "idle", false, "feat-x", "w1:plh-42", Some("w1:tlh-42"))],
                "retired": [member("old", "unknown", false, "old-wt", "w1:pold", None)],
                "strays": {"count": 1, "agents": [{"pane": "w2:p7", "tab": "w2:t7", "status": "idle",
                    "cwd": "/src/elsewhere", "title": "poking around"}]},
            })
        );

        let default = status(&herdr(), &ledger, Options::default(), Format::Json).unwrap();
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
                    status(&herdr, &ledger, Options { all, strays }, format).unwrap();
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
        let out = status(&herdr, &ledger, Options::default(), PLAIN).unwrap();
        assert_eq!(
            out.stdout.lines().nth(1),
            Some("  lh-1  idle  -  -  w1:tlh-1/w1:plh-1")
        );
        let json = status(&herdr, &ledger, Options::default(), Format::Json).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        let member = &json["ungrouped"][0];
        assert_eq!(
            (&member["branch"], &member["worktree"]),
            (&serde_json::Value::Null, &serde_json::Value::Null)
        );
    }
}
