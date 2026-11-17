//! Whether a Claude pane shows a subagent's view, where typed text and
//! `/clear` reach the subagent instead of the main conversation.

use anyhow::{Context, Result};

use crate::herdr::Herdr;

/// The refusal [`ensure_main_view`] returns, so a caller can tell it from a
/// failed read with `anyhow::Error::is`.
#[derive(Debug)]
pub struct ShowsSubagentView {
    name: String,
    pane: String,
    consequence: String,
    retry: String,
}

impl std::fmt::Display for ShowsSubagentView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ShowsSubagentView {
            name,
            pane,
            consequence,
            retry,
        } = self;
        write!(
            f,
            "{name}'s pane {pane} is showing a subagent's view, {consequence}: switch the pane \
             back to the main conversation, then {retry}"
        )
    }
}

impl std::error::Error for ShowsSubagentView {}

/// Refuses when `name`'s pane shows a subagent's view, saying what sending
/// there would do (`consequence`), to switch it back, and then `retry`.
pub fn ensure_main_view(
    herdr: &dyn Herdr,
    name: &str,
    pane: &str,
    consequence: &str,
    retry: &str,
) -> Result<()> {
    let screen = herdr
        .visible_text(pane)
        .with_context(|| format!("read {name}'s pane {pane} to check for a subagent view"))?;
    if shows_subagent_view(&screen) {
        return Err(ShowsSubagentView {
            name: name.into(),
            pane: pane.into(),
            consequence: consequence.into(),
            retry: retry.into(),
        }
        .into());
    }
    Ok(())
}

/// What text typed into a subagent's view does.
pub const TEXT_REACHES_SUBAGENT: &str = "so text sent there would reach the subagent";

/// Looks only at Claude's live prompt box (between the last two rules) and the
/// footer below it, since transcript output above can quote either. Without
/// that box, the prompt is the lowest `❯` line and the footer is what follows.
/// A subagent view addresses the subagent there, or its picker has an entry
/// other than `main` selected.
fn shows_subagent_view(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(unboxed).collect();
    let is_rule = |line: &&str| !line.is_empty() && line.chars().all(|c| c == '─');
    let rules = lines.iter().rposition(is_rule).and_then(|bottom| {
        let top = lines[..bottom].iter().rposition(is_rule)?;
        Some((top, bottom))
    });
    let (prompt, footer) = match rules {
        Some((top, bottom)) => (&lines[top + 1..bottom], &lines[bottom + 1..]),
        None => match lines.iter().rposition(|l| l.starts_with('❯')) {
            Some(at) => (&lines[at..=at], &lines[at..]),
            None => return false,
        },
    };
    prompt.iter().any(|line| line.starts_with("❯ Message @"))
        || footer
            .split(|line| picker_entry(line).is_none())
            .any(|run| {
                let entries = || run.iter().filter_map(|line| picker_entry(line));
                entries().any(|(_, entry)| entry == "main")
                    && entries().any(|(selected, entry)| selected && entry != "main")
            })
}

/// `(selected, entry)` for an agent picker line, `◯ entry` or `● entry`.
fn picker_entry(line: &str) -> Option<(bool, &str)> {
    let (selected, entry) = match line.strip_prefix("● ") {
        Some(entry) => (true, entry),
        None => (false, line.strip_prefix("◯ ")?),
    };
    Some((selected, entry.trim()))
}

/// A screen line without its indent or a box's `│` borders.
fn unboxed(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || c == '│')
}

#[cfg(test)]
pub mod screens {
    pub const SUBAGENT_PROMPT: &str = "\
● Reviewing PR #97 against the card.

────────────────────────────────────────
❯ Message @reviewer…
────────────────────────────────────────
  ← 1 agent
";

    pub const SUBAGENT_PICKER: &str = "\
● Reviewing PR #97 against the card.

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ◯ main
  ● local_agent · Review PR #97 …
";

    pub const MAIN_VIEW: &str = "\
● Handoff written; finished.

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ◯ local_agent · Review PR #97 …
  ● main
";
}
