//! `loopherd retire`: mark a ledger entry finished so revive leaves it alone.

use std::time::SystemTime;

use anyhow::{Result, anyhow, bail};

use crate::ledger::Ledger;

/// Marks `name` retired at `now`. Touches only the ledger.
pub fn retire(ledger: &mut dyn Ledger, name: &str, now: SystemTime) -> Result<()> {
    let mut entries = ledger.entries()?;
    let record = entries
        .get_mut(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.retired {
        bail!(
            "{name} is already retired (since {})",
            record.retired_at.as_deref().unwrap_or("an unknown time")
        );
    }
    record.retired = true;
    record.retired_at = Some(humantime::format_rfc3339_seconds(now).to_string());
    ledger.record(record.clone())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::herdr::Herdr;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::spawn::{SpawnRequest, spawn};
    use crate::worktree::testing::{current_branch, repo_with_origin};

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("abc".into()),
            repo: "/src/repo".into(),
            worktree: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
        }
    }

    fn ledger_with(names: &[&str]) -> MemLedger {
        MemLedger(names.iter().map(|n| (n.to_string(), record(n))).collect())
    }

    fn halloween_noon() -> SystemTime {
        humantime::parse_rfc3339("2026-10-31T12:00:00Z").unwrap()
    }

    #[test]
    fn marks_the_entry_retired_with_the_time() {
        let mut ledger = ledger_with(&["a", "b"]);
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let a = &ledger.0["a"];
        assert!(a.retired);
        assert_eq!(a.retired_at.as_deref(), Some("2026-10-31T12:00:00Z"));
        assert_eq!(ledger.0["b"], record("b"));
    }

    #[test]
    fn an_unknown_name_fails_and_leaves_the_ledger_untouched() {
        let mut ledger = ledger_with(&["a"]);
        let err = retire(&mut ledger, "ghost", halloween_noon()).unwrap_err();
        assert_eq!(format!("{err:#}"), "ghost is not in the ledger");
        assert_eq!(ledger.0, ledger_with(&["a"]).0);
    }

    #[test]
    fn an_already_retired_name_fails_and_keeps_its_first_retirement() {
        let mut ledger = ledger_with(&["a"]);
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let before = ledger.0.clone();
        let later = halloween_noon() + Duration::from_secs(3600);
        let err = retire(&mut ledger, "a", later).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "a is already retired (since 2026-10-31T12:00:00Z)"
        );
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn retiring_a_spawned_agent_leaves_its_tab_agent_and_worktree_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo,
            name: "a".into(),
            worktree: Some("feature/existing".into()),
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        let (tabs, agents) = (herdr.tabs(), herdr.agents().unwrap());

        retire(&mut ledger, "a", halloween_noon()).unwrap();

        assert!(ledger.0["a"].retired);
        assert_eq!(herdr.tabs(), tabs);
        assert_eq!(herdr.agents().unwrap(), agents);
        let wt = root.path().join("proj/worktrees/feature-existing");
        assert_eq!(current_branch(&wt), "feature/existing");
    }
}
