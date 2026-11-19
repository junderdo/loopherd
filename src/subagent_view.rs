//! Reading Claude's prompt box off a pane's screen: whether it shows a
//! subagent's view, where typed text and `/clear` reach the subagent instead
//! of the main conversation, and whether anything is typed in it.

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
pub fn shows_subagent_view(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(unboxed).collect();
    let Some((prompt, footer)) = prompt_box(&lines) else {
        return false;
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

/// Whether Claude's input box on `screen` holds nothing typed: its `❯`
/// line bare and any lines after it in the box blank. False when there's no box.
pub fn input_box_empty(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(unboxed).collect();
    let Some((prompt, _)) = prompt_box(&lines) else {
        return false;
    };
    let mut typed = prompt.iter().skip_while(|line| !line.starts_with('❯'));
    typed.next().is_some_and(|first| first == &"❯") && typed.all(|line| line.is_empty())
}

/// Claude's live prompt box (between the last two rules) and the footer
/// below it; without that box, the lowest `❯` line and what follows it.
fn prompt_box<'a>(lines: &'a [&'a str]) -> Option<(&'a [&'a str], &'a [&'a str])> {
    let is_rule = |line: &&str| !line.is_empty() && line.chars().all(|c| c == '─');
    let rules = lines.iter().rposition(is_rule).and_then(|bottom| {
        let top = lines[..bottom].iter().rposition(is_rule)?;
        Some((top, bottom))
    });
    match rules {
        Some((top, bottom)) => Some((&lines[top + 1..bottom], &lines[bottom + 1..])),
        None => {
            let at = lines.iter().rposition(|l| l.starts_with('❯'))?;
            Some((&lines[at..=at], &lines[at..]))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::screens::{MAIN_VIEW, SUBAGENT_PROMPT};
    use super::*;

    #[test]
    fn an_input_box_is_empty_only_with_nothing_typed_in_it() {
        assert!(input_box_empty(MAIN_VIEW));
        let boxed = "│ ● done\n│ ───────\n│ ❯ \n│ ───────\n";
        assert!(input_box_empty(boxed));

        let typed = MAIN_VIEW.replace("❯ \n", "❯ wait, one more thing\n");
        assert!(!input_box_empty(&typed));
        let second_line = MAIN_VIEW.replace("❯ \n", "❯ \n  and this\n");
        assert!(!input_box_empty(&second_line));
        assert!(!input_box_empty(SUBAGENT_PROMPT));
        assert!(!input_box_empty("● Working…\n"));
    }

    #[test]
    fn quoted_rules_and_prompts_above_the_box_dont_count() {
        let screen = format!("❯ an old prompt\n─────\n❯ quoted\n─────\n\n{MAIN_VIEW}");
        assert!(input_box_empty(&screen));
    }
}
