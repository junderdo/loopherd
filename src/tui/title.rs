//! A herd box's title: its open slots, pause and lead's switches.

use std::collections::BTreeMap;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::theme::Theme;
use crate::herd::{self, Outline, Paths};
use crate::herd_view::{Herd, View};

/// Short names for [`herd::Switches::named`], in its order.
const CHIPS: [&str; 6] = ["merge", "install", "rebase", "cards", "design", "dialogs"];

/// Each shown herd's outline, or why its herd file couldn't be read.
#[derive(Debug, Default)]
pub struct Outlines(BTreeMap<String, Result<Outline, String>>);

impl Outlines {
    pub fn read(paths: &Paths, view: &View) -> Outlines {
        Outlines(
            view.herds
                .iter()
                .map(|h| {
                    let outline = herd::outline(paths, &h.name).map_err(|e| format!("{e:#}"));
                    (h.name.clone(), outline)
                })
                .collect(),
        )
    }

    /// One line per herd file that couldn't be read, then `config.toml`'s
    /// problem once however many herds hit it.
    pub fn problems(&self) -> Vec<String> {
        let mut config = std::collections::BTreeSet::new();
        let mut lines = vec![];
        for (herd, outline) in &self.0 {
            match outline {
                Err(e) => lines.push(format!("{herd}: {e}")),
                Ok(outline) => config.extend(outline.defaults_problem.clone()),
            }
        }
        lines.extend(config.into_iter().map(|e| format!("lead switches: {e}")));
        lines
    }

    pub fn title(&self, herd: &Herd, theme: Theme) -> Line<'static> {
        let outline = self.0.get(&herd.name);
        let slots = match outline {
            Some(Ok(Outline { max: Some(max), .. })) => {
                format!(" · {}/{max} slots", herd.slots)
            }
            Some(Ok(_)) => String::new(),
            None | Some(Err(_)) => " · ? slots".into(),
        };
        let mut spans = vec![Span::styled(
            format!(" {}{slots}", herd.name),
            Modifier::BOLD,
        )];
        if herd.members.is_empty() && herd.paused.is_none() {
            spans.push(Span::styled(" (retired only)", Modifier::BOLD));
        }
        if let Some(Ok(Outline {
            switches: Some(switches),
            ..
        })) = outline
        {
            for (i, (chip, (_, on))) in CHIPS.into_iter().zip(switches.named()).enumerate() {
                spans.push(Span::raw(if i == 0 { "  " } else { " " }));
                spans.push(Span::styled(chip, theme.toggle(on)));
            }
        }
        // Last, since a long reason is cut off at the box's edge.
        if let Some(pause) = &herd.paused {
            let reason = pause
                .shown_reason()
                .map_or(String::new(), |r| format!(" — {r}"));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("⏸ paused{reason}"), theme.paused()));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}
