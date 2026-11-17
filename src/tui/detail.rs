//! The detail pane's lines for the selected member or stray, wrapped to the
//! pane and cut with `…` when they overflow it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::mail::Mail;
use super::theme::Theme;
use crate::herd_view::{Member, Stray};
use crate::report::{Body, Stop};

/// `time` is how long it has held its status, blank when unseen.
pub fn member(member: &Member, mail: &Mail, time: &str, theme: Theme) -> Vec<Line<'static>> {
    let status = match member.gone {
        true => "gone",
        false => member.status.as_str(),
    };
    let role = if member.lead { "lead" } else { "worker" };
    let mut lines = vec![
        Line::raw(format!("{status} {time}").trim_end().to_string()),
        Line::raw(match &member.template {
            Some(template) => format!("{template} · {role}"),
            None => role.into(),
        }),
    ];
    lines.extend(
        member
            .branch
            .iter()
            .map(|b| Line::raw(format!("branch {b}"))),
    );
    lines.extend(
        member
            .worktree_path
            .iter()
            .map(|w| Line::raw(format!("worktree {}", w.display()))),
    );
    let (report, stop) = mail.latest(&member.name);
    if let Some(entry) = stop
        && let Body::Stop(s) = &entry.body
    {
        lines.push(Line::raw(format!("stop {} {}", entry.when(), stopped(s))));
    }
    if let Some(entry) = report
        && let Some((kind, text)) = entry.report()
    {
        let heading = format!("{} {}", kind.as_str(), entry.when());
        lines.push(Line::default());
        lines.push(Line::styled(heading, theme.report(kind)));
        lines.extend(text.lines().map(linked));
    }
    lines
}

pub fn stray(stray: &Stray) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(format!("pane {}", stray.pane)),
        Line::raw(format!("tab {}", stray.tab)),
    ];
    lines.extend(
        stray
            .cwd
            .iter()
            .map(|c| Line::raw(format!("cwd {}", c.display()))),
    );
    lines.extend(stray.title.iter().map(|t| Line::raw(format!("title {t}"))));
    lines
}

/// `finished`, `paused 1 shell, 2 subagents`, or `failed rate_limit`.
fn stopped(stop: &Stop) -> String {
    match (&stop.error, stop.pending()) {
        (Some(error), _) => format!("failed {error}"),
        (None, Some(pending)) => format!("paused {pending}"),
        (None, None) => "finished".into(),
    }
}

/// `text` with its URLs underlined.
fn linked(text: &str) -> Line<'static> {
    let spans = text.split_inclusive(' ').map(|word| {
        let url = word.starts_with("https://") || word.starts_with("http://");
        match url {
            true => {
                let bare = word.trim_end();
                Line::from(vec![
                    Span::styled(bare.to_string(), Modifier::UNDERLINED),
                    Span::raw(word[bare.len()..].to_string()),
                ])
            }
            false => Line::raw(word.to_string()),
        }
    });
    Line::from(spans.flat_map(|l| l.spans).collect::<Vec<_>>())
}

/// `lines` word-wrapped to `width` columns and cut to `height`, the last
/// kept line ending in `…` when any were cut.
pub fn fit(lines: Vec<Line<'static>>, width: u16, height: u16) -> Vec<Line<'static>> {
    let width = (width as usize).max(1);
    let mut rows: Vec<Line<'static>> = lines.into_iter().flat_map(|l| wrap(l, width)).collect();
    let height = height as usize;
    if rows.len() > height {
        rows.truncate(height);
        if let Some(last) = rows.last_mut() {
            *last = ellipsized(std::mem::take(last), width);
        }
    }
    rows
}

fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows = vec![];
    let mut row: Vec<Span<'static>> = vec![];
    let mut used = 0;
    for span in line.spans {
        let style = line.style.patch(span.style);
        for word in span.content.split_inclusive(' ') {
            let word_width = columns(word.trim_end());
            if used > 0 && used + word_width > width {
                rows.push(Line::from(std::mem::take(&mut row)));
                used = 0;
            }
            for c in word.chars() {
                let w = columns(&c.to_string());
                if used + w > width {
                    if c == ' ' {
                        continue;
                    }
                    rows.push(Line::from(std::mem::take(&mut row)));
                    used = 0;
                }
                push(&mut row, c, style);
                used += w;
            }
        }
    }
    rows.push(Line::from(row));
    rows
}

/// Appends `c` to the last span when it shares `style`.
fn push(row: &mut Vec<Span<'static>>, c: char, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(c),
        _ => row.push(Span::styled(c.to_string(), style)),
    }
}

fn ellipsized(line: Line<'static>, width: usize) -> Line<'static> {
    let mut row = vec![];
    let mut used = 0;
    'cut: for span in line.spans {
        for c in span.content.chars() {
            let w = columns(&c.to_string());
            if used + w + 1 > width {
                break 'cut;
            }
            push(&mut row, c, span.style);
            used += w;
        }
    }
    row.push(Span::raw("…"));
    Line::from(row)
}

fn columns(s: &str) -> usize {
    Span::raw(s).width()
}
