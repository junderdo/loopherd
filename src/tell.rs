//! `loopherd tell`: message a live member, refusing while its pane shows a
//! subagent's view.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::herdr::{AgentStatus, Herdr, WaitOutcome};
use crate::subagent_view::ensure_main_view;

/// Sends `text` to `name` after checking its pane shows the main
/// conversation, then waits up to `timeout` for it to go `working`.
pub fn tell(herdr: &dyn Herdr, name: &str, text: &str, timeout: Duration) -> Result<String> {
    let agent = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("no live agent is named {name}"))?;
    let retry = format!("run loopherd tell {name} again");
    if agent.status == AgentStatus::Blocked {
        bail!(
            "{name} is blocked: answer it in pane {}, then {retry}",
            agent.pane_id
        );
    }
    ensure_main_view(herdr, name, &agent.pane_id, &retry)?;
    herdr.prompt(name, text)?;
    Ok(match herdr.wait(name, &[AgentStatus::Working], timeout)? {
        WaitOutcome::Reached => format!("told {name}; it's working"),
        WaitOutcome::TimedOut => format!(
            "told {name}, but it wasn't working after {}; check its pane {}",
            humantime::format_duration(timeout),
            agent.pane_id
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::subagent_view::screens::{MAIN_VIEW, SUBAGENT_PICKER, SUBAGENT_PROMPT};

    const PANE: &str = "w9:p9";
    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn sends_on_the_main_view_and_confirms_it_went_working() {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        herdr.set_screen(PANE, MAIN_VIEW);
        herdr.set_working_on_prompt();
        let out = tell(&herdr, "lh-1", "rebase onto main", WAIT).unwrap();
        assert_eq!(out, "told lh-1; it's working");
        assert_eq!(
            herdr.prompts(),
            vec![("lh-1".into(), "rebase onto main".into())]
        );
    }

    #[test]
    fn a_pane_showing_a_subagent_view_is_refused_and_sent_nothing() {
        for screen in [SUBAGENT_PROMPT, SUBAGENT_PICKER] {
            let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
            herdr.set_screen(PANE, screen);
            let err = tell(&herdr, "lh-1", "rebase onto main", WAIT).unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                "lh-1's pane w9:p9 is showing a subagent's view, so text sent there would reach the \
                 subagent: switch the pane back to the main conversation, then run loopherd tell lh-1 again"
            );
            assert!(herdr.prompts().is_empty(), "{screen}");
        }
    }

    #[test]
    fn says_so_when_the_member_did_not_go_working() {
        let herdr = FakeHerdr::new(&[]).with_agent_in("lh-1", AgentStatus::Idle);
        herdr.set_screen(PANE, MAIN_VIEW);
        let out = tell(&herdr, "lh-1", "rebase onto main", WAIT).unwrap();
        assert_eq!(
            out,
            "told lh-1, but it wasn't working after 10s; check its pane w9:p9"
        );
        assert_eq!(herdr.prompts().len(), 1);
    }

    #[test]
    fn a_blocked_or_missing_member_is_refused() {
        let herdr = FakeHerdr::new(&[]).with_agent_in("lh-1", AgentStatus::Blocked);
        let err = tell(&herdr, "lh-1", "hi", WAIT).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "lh-1 is blocked: answer it in pane w9:p9, then run loopherd tell lh-1 again"
        );
        let err = tell(&herdr, "lh-2", "hi", WAIT).unwrap_err();
        assert_eq!(format!("{err:#}"), "no live agent is named lh-2");
        assert!(herdr.prompts().is_empty());
    }
}
