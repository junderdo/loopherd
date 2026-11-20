//! `loopherd read`: a member's recent output, as `herdr agent read` gives it.

use anyhow::{Result, anyhow};

use crate::broker::MAX_READ_LINES;
use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::matching::live_agent;

/// How many lines `loopherd read` gives unless asked for more.
pub const DEFAULT_LINES: usize = 40;

/// The last `lines` lines of member `name`'s live agent, unwrapped, at most
/// [`MAX_READ_LINES`]. Read-only.
pub fn read(herdr: &dyn Herdr, ledger: &dyn Ledger, name: &str, lines: usize) -> Result<String> {
    let record = ledger
        .entries()?
        .remove(name)
        .ok_or_else(|| anyhow!("no member is named {name}"))?;
    let agents = herdr.agents()?;
    let agent =
        live_agent(&record, &agents).ok_or_else(|| anyhow!("no live agent matches {name}"))?;
    herdr.recent_text(&agent.pane_id, lines.clamp(1, MAX_READ_LINES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};

    fn ledger(names: &[&str]) -> MemLedger {
        let mut ledger = MemLedger::default();
        for name in names {
            let record: AgentRecord = serde_json::from_value(serde_json::json!({
                "name": name, "pane_id": "w9:p9", "session_id": null, "repo": "/r",
                "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
                "created": "2026-11-01T00:00:00Z",
            }))
            .unwrap();
            ledger.0.insert(name.to_string(), record);
        }
        ledger
    }

    #[test]
    fn gives_the_members_last_lines_capped_and_changes_nothing() {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        let screen: String = (1..=1500).map(|n| format!("line {n}\n")).collect();
        herdr.set_screen("w9:p9", &screen);
        let ledger = ledger(&["lh-1"]);

        assert_eq!(
            read(&herdr, &ledger, "lh-1", 2).unwrap(),
            "line 1499\nline 1500\n"
        );
        let capped = read(&herdr, &ledger, "lh-1", 5000).unwrap();
        assert_eq!(capped.lines().count(), MAX_READ_LINES);
        assert_eq!(herdr.mutations(), 0);
    }

    #[test]
    fn a_name_the_ledger_lacks_or_with_no_live_agent_is_named_in_the_error() {
        let herdr = FakeHerdr::new(&[]).with_agent("lh-1");
        let ledger = ledger(&["lh-2"]);
        for (name, why) in [
            ("lh-3", "no member is named lh-3"),
            ("lh-2", "no live agent matches lh-2"),
        ] {
            let err = read(&herdr, &ledger, name, 40).unwrap_err();
            assert_eq!(format!("{err:#}"), why);
        }
    }
}
