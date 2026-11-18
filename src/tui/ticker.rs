//! The strip above the footer showing the newest mailbox events.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::detail::{ellipsized, stopped};
use super::theme::Theme;
use crate::report::{Body, Entry};

/// `<HH:MM> <member> <kind> <first line>`, cut to `width`, bold when `fresh`.
pub fn line(name: &str, entry: &Entry, fresh: bool, width: u16, theme: Theme) -> Line<'static> {
    let (kind, kind_style, text) = match (&entry.body, entry.report()) {
        (_, Some((kind, text))) => (
            kind.as_str(),
            theme.report(kind).remove_modifier(Modifier::BOLD),
            text.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .replace('\t', "    "),
        ),
        (Body::Stop(stop), None) => (
            "stop",
            Style::new().add_modifier(Modifier::DIM),
            stopped(stop),
        ),
        (Body::Blocked {}, None) => ("blocked", Style::new(), String::new()),
        (Body::Gone { .. }, None) => ("gone", Style::new(), String::new()),
        _ => ("", Style::new(), String::new()),
    };
    let minute = entry.at.get(11..16).unwrap_or(&entry.at);
    let weight = match fresh {
        true => Modifier::BOLD,
        false => Modifier::empty(),
    };
    let line = Line::from(vec![
        Span::styled(format!("{minute} {name} "), weight),
        Span::styled(kind, kind_style.add_modifier(weight)),
        Span::styled(format!(" {text}"), weight),
    ]);
    match line.width() > width as usize {
        true => ellipsized(line, width as usize),
        false => line,
    }
}
