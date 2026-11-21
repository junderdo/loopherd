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
    let status = crate::status::label(member);
    let role = match (member.lead, &member.unsandboxed) {
        (true, Some(why)) => format!("lead, unsandboxed: {why}"),
        (true, None) => match &member.sockets_unblocked {
            Some(why) => format!("lead, Unix sockets open: {why}"),
            None => "lead".into(),
        },
        (false, Some(why)) => format!("worker, unsandboxed: {why}"),
        (false, None) if member.attended => "attended".into(),
        (false, None) => "worker".into(),
    };
    let mut lines = vec![
        Line::raw(format!("{status} {time}").trim_end().to_string()),
        Line::raw(match &member.template {
            Some(template) => format!("{template} · {role}"),
            None => role,
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
    lines.push(Line::raw(match &member.tab {
        Some(tab) => format!("pane {tab}/{}", member.pane),
        None => format!("pane {}", member.pane),
    }));
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

/// `finished`, `paused 1 shell, 2 subagents`, `background unknown`, or
/// `failed rate_limit`.
pub fn stopped(stop: &Stop) -> String {
    if let Some(error) = &stop.error {
        return format!("failed {error}");
    }
    match stop.in_flight() {
        (None, None) => "finished".into(),
        (pending, unknown) => pending
            .map(|p| format!("paused {p}"))
            .into_iter()
            .chain(unknown)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// `text` with its URLs underlined, less any punctuation closing them.
fn linked(text: &str) -> Line<'static> {
    let mut spans = vec![];
    for word in text.split_inclusive(' ') {
        let Some(start) = word.find("https://").or_else(|| word.find("http://")) else {
            spans.push(Span::raw(word.to_string()));
            continue;
        };
        let rest = &word[start..];
        let url = without_closing_punctuation(rest);
        spans.push(Span::raw(word[..start].to_string()));
        spans.push(Span::styled(url.to_string(), Modifier::UNDERLINED));
        spans.push(Span::raw(rest[url.len()..].to_string()));
    }
    Line::from(spans)
}

/// `url` without trailing punctuation, keeping a `)` that closes a `(`
/// inside it.
fn without_closing_punctuation(url: &str) -> &str {
    let mut url = url.trim_end();
    while let Some(last) = url.chars().last() {
        let unmatched = url.matches('(').count() < url.matches(')').count();
        let closing = match last {
            ')' => unmatched,
            c => ".,;:!?]}>'\"".contains(c),
        };
        if !closing {
            break;
        }
        url = &url[..url.len() - 1];
    }
    url
}

/// `lines` word-wrapped to `width` columns and cut to `height`, the last
/// kept line ending in `…` when any were cut.
pub fn fit(lines: Vec<Line<'static>>, width: u16, height: u16) -> Vec<Line<'static>> {
    let width = (width as usize).max(1);
    let mut rows: Vec<Line<'static>> = lines
        .into_iter()
        .map(tabs_as_spaces)
        .flat_map(|l| wrap(l, width))
        .collect();
    let height = height as usize;
    if rows.len() > height {
        rows.truncate(height);
        if let Some(last) = rows.last_mut() {
            *last = ellipsized(std::mem::take(last), width);
        }
    }
    rows
}

/// A tab draws as nothing, running the words around it together.
fn tabs_as_spaces(mut line: Line<'static>) -> Line<'static> {
    for span in &mut line.spans {
        if span.content.contains('\t') {
            span.content = span.content.replace('\t', "    ").into();
        }
    }
    line
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

pub fn ellipsized(line: Line<'static>, width: usize) -> Line<'static> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentStatus;

    #[test]
    fn an_attended_worker_running_unsandboxed_is_marked_unsandboxed_as_status_marks_it() {
        let worker = Member {
            name: "grill-1".into(),
            status: AgentStatus::Idle,
            gone: false,
            branch: None,
            worktree: None,
            worktree_path: None,
            pane: "w1:p1".into(),
            tab: None,
            lead: false,
            attended: true,
            unsandboxed: Some("bubblewrap not installed".into()),
            sockets_unblocked: None,
            template: None,
            released: false,
            retired: None,
        };

        let lines = member(&worker, &Mail::default(), "", Theme::from_env(None, true));

        assert_eq!(
            lines[1].to_string(),
            "worker, unsandboxed: bubblewrap not installed"
        );
        assert_eq!(crate::status::row(&worker)[0], "grill-1 (unsandboxed)");
    }
}
