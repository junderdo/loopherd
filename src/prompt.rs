//! `loopherd prompt`: send a ledger entry's stored prompt that spawn couldn't.

use anyhow::{Result, anyhow, bail};

use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::subagent_view::ensure_main_view;

/// Sends `name`'s stored prompt to its live agent and marks it sent,
/// refusing a pane that shows a subagent's view.
pub fn prompt(herdr: &dyn Herdr, ledger: &mut dyn Ledger, name: &str, force: bool) -> Result<()> {
    let record = ledger
        .entries()?
        .remove(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.prompt_sent && !force {
        bail!("{name}'s prompt was already sent; pass --force to send it again");
    }
    let text = record
        .prompt
        .clone()
        .ok_or_else(|| anyhow!("{name} has no stored prompt"))?;
    let agent = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("no live agent is named {name}"))?;
    if agent.status == AgentStatus::Blocked {
        bail!(
            "{name} is blocked: answer it in pane {}, then run loopherd prompt {name} again",
            agent.pane_id
        );
    }
    ensure_main_view(
        herdr,
        name,
        &agent.pane_id,
        &format!("run loopherd prompt {name} again"),
    )?;
    herdr.prompt(name, &text)?;
    ledger
        .update(name, &mut |record| {
            record.prompt_sent = true;
            Ok(())
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::subagent_view::screens::{SUBAGENT_PICKER, SUBAGENT_PROMPT};

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w9:p9".into(),
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w9".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: Some("start work".into()),
            prompt_sent: false,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
        }
    }

    fn ledger_with(record: AgentRecord) -> MemLedger {
        MemLedger([(record.name.clone(), record)].into(), vec![])
    }

    #[test]
    fn sends_the_stored_prompt_and_marks_it_sent() {
        let herdr = FakeHerdr::new(&[]).with_agent("a");
        let mut ledger = ledger_with(record("a"));
        prompt(&herdr, &mut ledger, "a", false).unwrap();
        assert_eq!(herdr.prompts(), vec![("a".into(), "start work".into())]);
        assert!(ledger.0["a"].prompt_sent);
    }

    fn assert_refused(herdr: &FakeHerdr, ledger: MemLedger, name: &str, force: bool, msg: &str) {
        let before = ledger.0.clone();
        let mut ledger = ledger;
        let err = prompt(herdr, &mut ledger, name, force).unwrap_err();
        assert_eq!(format!("{err:#}"), msg);
        assert_eq!(ledger.0, before);
        assert!(herdr.prompts().is_empty());
    }

    #[test]
    fn an_unknown_name_is_refused() {
        let herdr = FakeHerdr::new(&[]).with_agent("ghost");
        assert_refused(
            &herdr,
            ledger_with(record("a")),
            "ghost",
            false,
            "ghost is not in the ledger",
        );
    }

    #[test]
    fn an_already_sent_prompt_is_refused_without_force() {
        let herdr = FakeHerdr::new(&[]).with_agent("a");
        let sent = AgentRecord {
            prompt_sent: true,
            ..record("a")
        };
        assert_refused(
            &herdr,
            ledger_with(sent),
            "a",
            false,
            "a's prompt was already sent; pass --force to send it again",
        );
    }

    #[test]
    fn force_sends_an_already_sent_prompt_again() {
        let herdr = FakeHerdr::new(&[]).with_agent("a");
        let sent = AgentRecord {
            prompt_sent: true,
            ..record("a")
        };
        let mut ledger = ledger_with(sent);
        prompt(&herdr, &mut ledger, "a", true).unwrap();
        assert_eq!(herdr.prompts(), vec![("a".into(), "start work".into())]);
        assert!(ledger.0["a"].prompt_sent);
    }

    #[test]
    fn a_name_with_no_live_agent_is_refused() {
        let herdr = FakeHerdr::new(&[]).with_agent("other");
        assert_refused(
            &herdr,
            ledger_with(record("a")),
            "a",
            false,
            "no live agent is named a",
        );
    }

    #[test]
    fn a_blocked_agent_is_refused_and_its_dialog_left_alone() {
        let herdr = FakeHerdr::new(&[]).with_agent_in("a", AgentStatus::Blocked);
        assert_refused(
            &herdr,
            ledger_with(record("a")),
            "a",
            true,
            "a is blocked: answer it in pane w9:p9, then run loopherd prompt a again",
        );
        assert_eq!(herdr.agent("a").unwrap().status, AgentStatus::Blocked);
    }

    #[test]
    fn an_entry_without_a_stored_prompt_is_refused() {
        let herdr = FakeHerdr::new(&[]).with_agent("a");
        let none = AgentRecord {
            prompt: None,
            ..record("a")
        };
        assert_refused(
            &herdr,
            ledger_with(none),
            "a",
            false,
            "a has no stored prompt",
        );
    }

    #[test]
    fn a_pane_showing_a_subagent_view_is_refused_even_with_force() {
        for screen in [SUBAGENT_PROMPT, SUBAGENT_PICKER] {
            let herdr = FakeHerdr::new(&[]).with_agent("a");
            herdr.set_screen("w9:p9", screen);
            assert_refused(
                &herdr,
                ledger_with(record("a")),
                "a",
                true,
                "a's pane w9:p9 is showing a subagent's view, so text sent there would reach the \
                 subagent: switch the pane back to the main conversation, then run loopherd prompt a again",
            );
        }
    }
}
