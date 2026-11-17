//! `loopherd view`: the herd view, live.

mod detail;
mod mail;
mod since;
pub mod theme;
mod ticker;
mod title;
mod watcher;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use ratatui::{Frame, Terminal};

use crate::herd;
use crate::herd_view::{Member, Options, Stray};
use crate::herdr::Herdr;
use crate::herdr::events::Subscriber;
use crate::ledger::Ledger;
use crate::pause::Pauses;
use crate::status::{Read, aligned, read, row, sections, stray_lines, widths};
use mail::{Mail, Waiting, waiting};
use since::{Since, Staleness, staleness};
use theme::Theme;
use title::Outlines;
use watcher::{Signal, Watch, watch};

const POLL: Duration = Duration::from_secs(2);
/// The ledger and mailbox push no events, so a read still comes this often
/// while herdr's events cover agent status.
const SUBSCRIBED_POLL: Duration = Duration::from_secs(10);
/// How long the loop waits for a key before checking replies and signals.
const TICK: Duration = Duration::from_millis(100);
/// Wide enough for `for 59m`, so the columns don't jump when a time appears.
const TIME_WIDTH: usize = 7;
/// The narrowest terminal that shows the detail pane beside the list.
const DETAIL_MIN_WIDTH: u16 = 100;
/// The shortest terminal that shows the event ticker.
const TICKER_MIN_HEIGHT: u16 = 16;
/// Body rows below which the quiet herd drops its sheep for just the line.
const QUIET_SHEEP_MIN_HEIGHT: u16 = 10;
/// The install splash's largest sheep, grazing.
const SHEEP: [&str; 4] = [
    " ▄█▄█▄█▄█▄",
    "▐█████████▌",
    " ▀█████████▄▄",
    "  ▌▐    ▌▐▓o▌",
];
/// Over a member's cells, the time's column left blank.
const HEADINGS: [&str; 6] = ["NAME", "STATUS", "", "BRANCH", "WORKTREE", "PANE"];
/// Where the CONTEXT column goes when any row has a gauge: after the status time.
const GAUGE: usize = 3;

/// Work for the reader thread: anything that touches herdr or the ledger,
/// which can hang, so the input loop never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Read(Options),
    Focus(String),
}

pub enum Response {
    Read(Options, Result<(Read, Mail, Outlines)>),
    Focus(String, Result<()>),
}

/// Where the reader finds what isn't in herdr or the ledger.
pub struct Dirs {
    pub pauses: Pauses,
    pub mailboxes: PathBuf,
    pub herds: herd::Paths,
}

pub fn serve(request: Request, herdr: &dyn Herdr, ledger: &dyn Ledger, dirs: &Dirs) -> Response {
    match request {
        Request::Read(options) => {
            let read = read(herdr, ledger, &dirs.pauses, options).map(|read| {
                let mail = Mail::read(&dirs.mailboxes, &read.view);
                let outlines = Outlines::read(&dirs.herds, &read.view);
                (read, mail, outlines)
            });
            Response::Read(options, read)
        }
        Request::Focus(pane) => {
            let result = herdr.focus(&pane);
            Response::Focus(pane, result)
        }
    }
}

/// Serves requests in order on its own thread until the sender drops.
fn spawn_reader<H, L>(herdr: H, ledger: L, dirs: Dirs) -> (Sender<Request>, Receiver<Response>)
where
    H: Herdr + Send + 'static,
    L: Ledger + Send + 'static,
{
    let (requests, inbox) = mpsc::channel();
    let (outbox, responses) = mpsc::channel();
    std::thread::spawn(move || {
        for request in inbox {
            if outbox.send(serve(request, &herdr, &ledger, &dirs)).is_err() {
                break;
            }
        }
    });
    (requests, responses)
}

pub struct App {
    options: Options,
    theme: Theme,
    /// The first tick, from which the spinner's frame counts.
    opened: Option<Instant>,
    read: Option<Read>,
    mail: Mail,
    /// Every event read so far, by `Event::id`; `None` before the first read.
    seen_events: Option<HashSet<(String, usize, String)>>,
    /// Events that arrived with the last read, shown bold.
    fresh_events: HashSet<(String, usize, String)>,
    outlines: Outlines,
    since: Since,
    selected: usize,
    /// The selected row's identity, to keep it selected as rows re-sort.
    selected_key: Option<Key>,
    message: Option<String>,
    /// When the read in flight was requested.
    reading: Option<Instant>,
    /// When the last read was requested or arrived; `None` before the first.
    polled: Option<Instant>,
    /// An event arrived since the last read was requested.
    invalidated: bool,
    /// herdr's events are subscribed, so the poll slows.
    live: bool,
    quit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Member(String),
    Stray(String),
}

enum Target<'a> {
    Member(&'a Member),
    Stray(&'a Stray),
}

impl Target<'_> {
    fn key(&self) -> Key {
        match self {
            Target::Member(m) => Key::Member(m.name.clone()),
            Target::Stray(s) => Key::Stray(s.pane.clone()),
        }
    }
}

struct Row {
    line: Line<'static>,
    /// Index among the selectable rows.
    target: Option<usize>,
}

/// A titled box of rows: a herd, `ungrouped`, `retired` or the strays.
struct Panel {
    title: Line<'static>,
    rows: Vec<Row>,
}

impl Panel {
    fn height(&self) -> u16 {
        (self.rows.len() + 2).min(u16::MAX as usize) as u16
    }
}

impl App {
    pub fn new(options: Options, theme: Theme) -> App {
        App {
            options,
            theme,
            opened: None,
            read: None,
            mail: Mail::default(),
            seen_events: None,
            fresh_events: HashSet::new(),
            outlines: Outlines::default(),
            since: Since::default(),
            selected: 0,
            selected_key: None,
            message: None,
            reading: None,
            polled: None,
            invalidated: false,
            live: false,
            quit: false,
        }
    }

    /// A read, when one is due and none is in flight.
    pub fn tick(&mut self, now: Instant) -> Option<Request> {
        self.opened.get_or_insert(now);
        let due = self.invalidated || self.polled.is_none_or(|at| now >= at + self.poll());
        (due && self.reading.is_none()).then(|| self.request_read(now))
    }

    /// Something herdr shows may have changed: read again as soon as no read
    /// is in flight.
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Events cover agent status only when subscribed and the last read could
    /// see herdr's panes to subscribe to. A warning that isn't about herdr
    /// leaves the code at 0.
    fn poll(&self) -> Duration {
        let herdr_read = self.read.as_ref().is_some_and(|r| r.code == 0);
        if self.live && herdr_read {
            SUBSCRIBED_POLL
        } else {
            POLL
        }
    }

    fn request_read(&mut self, now: Instant) -> Request {
        self.reading = Some(now);
        self.invalidated = false;
        self.polled = Some(now);
        Request::Read(self.options)
    }

    pub fn apply(&mut self, response: Response, now: Instant) -> Result<()> {
        match response {
            // Read with options since toggled; the read for the new ones is queued.
            Response::Read(options, _) if options != self.options => {}
            Response::Read(_, result) => {
                self.reading = None;
                self.polled = Some(now);
                let (read, mail, outlines) = result?;
                self.since = self.since.observe(&read.view, now);
                let events: HashSet<_> = mail
                    .events(&read.view)
                    .into_iter()
                    .map(|e| e.id())
                    .collect();
                let seen = self.seen_events.get_or_insert_with(|| events.clone());
                self.fresh_events = events.difference(seen).cloned().collect();
                seen.extend(events);
                self.read = Some(read);
                self.mail = mail;
                self.outlines = outlines;
                let kept = self
                    .selected_key
                    .as_ref()
                    .and_then(|key| self.keys_in_order().iter().position(|k| k == key));
                self.select(kept.unwrap_or(self.selected));
            }
            Response::Focus(_, Ok(())) => {}
            Response::Focus(pane, Err(e)) => self.message = Some(format!("focus {pane}: {e:#}")),
        }
        Ok(())
    }

    pub fn press(&mut self, key: KeyCode, now: Instant) -> Option<Request> {
        self.message = None;
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('s') => {
                self.options.strays = !self.options.strays;
                return Some(self.request_read(now));
            }
            KeyCode::Char('a') => {
                self.options.all = !self.options.all;
                return Some(self.request_read(now));
            }
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Enter => return self.focus(),
            _ => {}
        }
        None
    }

    pub fn quit(&self) -> bool {
        self.quit
    }

    pub fn draw(&self, frame: &mut Frame, now: Instant) {
        let window = self.block().title(self.theme.wordmark(" loopherd "));
        let inner = window.inner(frame.area());
        frame.render_widget(window, frame.area());
        let notes: Vec<Vec<Line>> = self
            .notes(now)
            .iter()
            .map(|n| wrapped(n, inner.width.saturating_sub(1)))
            .collect();
        let ticker_height = match frame.area().height >= TICKER_MIN_HEIGHT {
            true => 4,
            false => 0,
        };
        // Notes keep half the space at most, and the newest whole notes when cut.
        let room = inner.height.saturating_sub(2 + ticker_height) / 2;
        let notes = newest_whole(notes, room as usize);
        let notes_height = notes.len();
        let [banner, body, notes_area, ticker_area, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(notes_height as u16),
            Constraint::Length(ticker_height),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(self.banner(banner.width), banner);
        let (list, detail) = match frame.area().width >= DETAIL_MIN_WIDTH {
            true => {
                let [list, detail] =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .areas(body);
                (list, Some(detail))
            }
            false => (body, None),
        };
        self.draw_panels(frame.buffer_mut(), list, now);
        if let Some(area) = detail {
            self.draw_detail(frame.buffer_mut(), area, now);
        }
        frame.render_widget(Paragraph::new(notes), notes_area);
        if ticker_height > 0 {
            self.draw_ticker(frame.buffer_mut(), ticker_area);
        }
        frame.render_widget(self.keys(), footer);
    }

    /// The two newest events in a dim box.
    fn draw_ticker(&self, out: &mut Buffer, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Modifier::DIM);
        let inner = block.inner(area);
        block.render(area, out);
        let text = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let Some(read) = &self.read else {
            return;
        };
        let lines: Vec<Line> = self
            .mail
            .events(&read.view)
            .into_iter()
            .take(2)
            .map(|e| {
                let fresh = self.fresh_events.contains(&e.id());
                ticker::line(e.name, e.entry, fresh, text.width, self.theme)
            })
            .collect();
        Paragraph::new(lines).render(text, out);
    }

    /// What waits on the human, naming as many as fit in `width`.
    fn banner(&self, width: u16) -> Line<'static> {
        let Some(read) = &self.read else {
            return Line::default();
        };
        let Waiting {
            questions,
            blocked,
            items,
        } = waiting(&read.view, &self.mail);
        // Without herdr's agents every status is unknown, blocked included.
        let statuses_read = read.code == 0;
        if items.is_empty() && statuses_read {
            return Line::styled(" nothing waiting on you", Modifier::DIM);
        }
        let blocked = match statuses_read {
            true => blocked.to_string(),
            false => "?".into(),
        };
        let plural = if questions == 1 { "" } else { "s" };
        let mut text = format!(" ⚑ {questions} question{plural} · {blocked} blocked");
        let width = width as usize;
        let columns = |s: &str| Span::raw(s).width();
        for (i, item) in items.iter().enumerate() {
            let separator = if i == 0 { " — " } else { ", " };
            let ellipsis = if i + 1 < items.len() {
                columns(" …")
            } else {
                0
            };
            let wanted = [&text, separator, item].map(columns);
            if wanted.iter().sum::<usize>() + ellipsis > width {
                text.push_str(" …");
                break;
            }
            text.push_str(separator);
            text.push_str(item);
        }
        Line::styled(text, self.theme.title())
    }

    fn block(&self) -> Block<'static> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border())
    }

    /// The panels stacked top to bottom and scrolled to keep the selected row
    /// in view. Panels below the view aren't drawn.
    fn draw_panels(&self, out: &mut Buffer, body: Rect, now: Instant) {
        let panels = self.panels(now);
        let mut top = 0u16;
        let mut selected_line = 0;
        for panel in &panels {
            if let Some(i) = panel
                .rows
                .iter()
                .position(|r| r.target == Some(self.selected))
            {
                selected_line = top.saturating_add(1 + i as u16);
            }
            top = top.saturating_add(panel.height());
        }
        let scroll = (selected_line + 1).saturating_sub(body.height);
        let bottom = scroll.saturating_add(body.height);
        let shown: Vec<Panel> = panels
            .into_iter()
            .scan(0u16, |top, panel| {
                let starts = *top;
                *top = top.saturating_add(panel.height());
                (starts < bottom).then_some(panel)
            })
            .collect();
        let height = shown.iter().fold(0u16, |h, p| h.saturating_add(p.height()));
        // Drawn whole off-screen, so a panel cut by the view keeps its borders.
        let mut page = Buffer::empty(Rect::new(0, 0, body.width, height));
        let mut y = 0;
        for panel in shown {
            let area = Rect::new(0, y, body.width, panel.height()).intersection(page.area);
            y = y.saturating_add(panel.height());
            let block = self.block().title(panel.title);
            let inner = block.inner(area);
            block.render(area, &mut page);
            for (row, y) in panel.rows.into_iter().zip(inner.y..inner.bottom()) {
                let line = match row.target == Some(self.selected) {
                    true => highlighted(row.line, inner.width, self.theme.selected()),
                    false => row.line,
                };
                line.render(
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                    &mut page,
                );
            }
        }
        for y in 0..height.saturating_sub(scroll).min(body.height) {
            for x in 0..body.width {
                out[(body.x + x, body.y + y)] = page[(x, scroll + y)].clone();
            }
        }
        if self.read.is_some() && self.targets().is_empty() {
            let below = Rect {
                y: body.y + height.min(body.height),
                height: body.height.saturating_sub(height),
                ..body
            };
            self.draw_quiet(out, below, body.height >= QUIET_SHEEP_MIN_HEIGHT);
        }
    }

    /// A grazing sheep from the install splash over `the herd is quiet`,
    /// centered in `area`.
    fn draw_quiet(&self, out: &mut Buffer, area: Rect, sheep: bool) {
        let mut lines: Vec<Line> = vec![];
        if sheep {
            let width = SHEEP.iter().map(|r| r.chars().count()).max().unwrap_or(0);
            lines.extend(
                SHEEP
                    .iter()
                    .map(|r| Line::styled(format!("{r:<width$}"), self.theme.sheep())),
            );
            lines.push(Line::default());
        }
        lines.push(Line::styled("the herd is quiet", self.theme.label()));
        let [area] = Layout::vertical([Constraint::Length(lines.len() as u16)])
            .flex(Flex::Center)
            .areas(area);
        Paragraph::new(lines).centered().render(area, out);
    }

    /// The selected agent's details in a box titled with its name.
    fn draw_detail(&self, out: &mut Buffer, area: Rect, now: Instant) {
        let targets = self.targets();
        let (title, lines) = match targets.get(self.selected) {
            Some(Target::Member(m)) => {
                let time = time(self.since.of(&m.name, now));
                let lines = detail::member(m, &self.mail, &time, self.theme);
                (m.name.as_str(), lines)
            }
            Some(Target::Stray(s)) => ("stray", detail::stray(s)),
            None => ("", vec![]),
        };
        let mut block = self.block();
        if !title.is_empty() {
            block = block.title(Span::styled(format!(" {title} "), self.theme.title()));
        }
        let inner = block.inner(area);
        block.render(area, out);
        let text = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };
        Paragraph::new(detail::fit(lines, text.width, text.height)).render(text, out);
    }

    fn keys(&self) -> Line<'static> {
        let t = self.theme;
        let mut spans = vec![];
        let key = |spans: &mut Vec<Span<'static>>, key: &'static str, label: &'static str| {
            spans.push(Span::raw(if spans.is_empty() { " " } else { "  " }));
            spans.push(Span::styled(key, t.key()));
            spans.push(Span::styled(format!(" {label}"), t.label()));
        };
        key(&mut spans, "q", "quit");
        key(&mut spans, "↑↓/jk", "select");
        key(&mut spans, "enter", "focus");
        for (k, label, on) in [
            ("s", "strays:", self.options.strays),
            ("a", "all:", self.options.all),
        ] {
            key(&mut spans, k, label);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(if on { "on" } else { "off" }, t.toggle(on)));
        }
        Line::from(spans)
    }

    fn select(&mut self, index: usize) {
        let keys = self.keys_in_order();
        self.selected = index.min(keys.len().saturating_sub(1));
        self.selected_key = keys.into_iter().nth(self.selected);
    }

    fn keys_in_order(&self) -> Vec<Key> {
        self.targets().iter().map(Target::key).collect()
    }

    /// A focus request for the selected agent, or the reason there's none.
    fn focus(&mut self) -> Option<Request> {
        let targets = self.targets();
        let refusal = match targets.get(self.selected)? {
            Target::Member(m) if m.gone => {
                format!("{} is gone; `loopherd revive` can bring it back", m.name)
            }
            // Only a live agent has a tab.
            Target::Member(m) if m.tab.is_none() => {
                format!("{} has no live pane to focus", m.name)
            }
            Target::Member(m) => return Some(Request::Focus(m.pane.clone())),
            Target::Stray(s) => return Some(Request::Focus(s.pane.clone())),
        };
        self.message = Some(refusal);
        None
    }

    /// The live panes the view shows, whose status changes it must see.
    pub fn panes(&self) -> Vec<String> {
        self.targets()
            .into_iter()
            .filter_map(|t| match t {
                Target::Member(m) => m.tab.as_ref().map(|_| m.pane.clone()),
                Target::Stray(s) => Some(s.pane.clone()),
            })
            .collect()
    }

    /// Members in display order, then listed strays.
    fn targets(&self) -> Vec<Target<'_>> {
        let Some(read) = &self.read else {
            return vec![];
        };
        let members = sections(&read.view)
            .into_iter()
            .flat_map(|(_, members)| members)
            .map(Target::Member);
        let strays = read.view.strays.agents.iter().flatten().map(Target::Stray);
        members.chain(strays).collect()
    }

    /// Lines under the panels: the gone legend, mail, outline and herdr problems,
    /// that the first read is pending or herdr is slow, and what the last key did.
    fn notes(&self, now: Instant) -> Vec<String> {
        let mut notes = vec![];
        if let Some(read) = &self.read {
            if sections(&read.view)
                .iter()
                .any(|(_, m)| m.iter().any(|m| m.gone))
            {
                notes.push(
                    "gone: no live agent matches it; `loopherd revive` can bring it back".into(),
                );
            }
            // Before herdr's problem, since the notes keep the newest when cut.
            notes.extend(self.mail.warnings.iter().cloned());
            notes.extend(self.outlines.problems());
            notes.extend(
                read.problem
                    .iter()
                    .flat_map(|p| p.lines())
                    .map(String::from),
            );
        }
        match (&self.read, self.reading) {
            (None, _) => notes.push("reading herdr and the ledger…".into()),
            (Some(_), Some(since)) if now.saturating_duration_since(since) >= POLL => {
                let waited = now.saturating_duration_since(since).as_secs();
                notes.push(format!(
                    "herdr hasn't answered for {waited}s; showing the last read"
                ));
            }
            _ => {}
        }
        notes.extend(self.message.clone());
        notes
    }

    fn panels(&self, now: Instant) -> Vec<Panel> {
        let Some(read) = &self.read else {
            return vec![];
        };
        let view = &read.view;
        let sections = sections(view);
        let gauges: Vec<Vec<Option<Span>>> = sections
            .iter()
            .map(|(_, members)| {
                members
                    .iter()
                    .map(|m| {
                        let (context, limit) = self.mail.context_use(&m.name)?;
                        Some(self.theme.gauge(context, limit))
                    })
                    .collect()
            })
            .collect();
        let gauged = gauges.iter().flatten().any(Option::is_some);
        let cells: Vec<Vec<Vec<String>>> = sections
            .iter()
            .zip(&gauges)
            .map(|((_, members), gauges)| {
                members
                    .iter()
                    .zip(gauges)
                    .map(|(m, gauge)| {
                        let mut cells = row(m);
                        cells.insert(2, time(self.since.of(&m.name, now)));
                        if gauged {
                            let gauge = gauge.as_ref().map(|g| g.content.to_string());
                            cells.insert(GAUGE, gauge.unwrap_or_default());
                        }
                        cells
                    })
                    .collect()
            })
            .collect();
        let mut headings: Vec<String> = HEADINGS.map(String::from).to_vec();
        if gauged {
            headings.insert(GAUGE, "CONTEXT".into());
        }
        let mut widths = widths(cells.iter().flatten().chain([&headings]));
        if let Some(w) = widths.get_mut(2) {
            *w = (*w).max(TIME_WIDTH);
        }
        let heading = Line::from(vec![
            Span::raw("   "),
            Span::styled(aligned(&headings, &widths), self.theme.heading()),
        ]);
        let spinner = self.opened.map_or(0, |at| {
            now.saturating_duration_since(at).as_millis() / TICK.as_millis()
        }) as usize;
        let mut panels = vec![];
        let mut target = 0;
        for (i, (((name, members), cells), gauges)) in
            sections.iter().zip(&cells).zip(gauges).enumerate()
        {
            let mut rows = vec![];
            if !members.is_empty() {
                rows.push(Row {
                    line: heading.clone(),
                    target: None,
                });
            }
            for ((member, cells), gauge) in members.iter().zip(cells).zip(gauges) {
                let (glyph, style) = self.theme.glyph(member.status, member.gone, spinner);
                let mut spans = vec![Span::raw(" "), Span::styled(glyph, style), Span::raw(" ")];
                let (text_style, time_style) = match member.gone {
                    true => (
                        Style::new().add_modifier(Modifier::DIM),
                        Modifier::DIM.into(),
                    ),
                    false => {
                        let time = self.since.of(&member.name, now);
                        let time_style = match time.map(|t| staleness(member, t)) {
                            Some(Staleness::Stale) => self.theme.stale(),
                            Some(Staleness::Overdue) => self.theme.overdue(),
                            _ => Style::new(),
                        };
                        (Style::new(), time_style)
                    }
                };
                let (before, after) = cells.split_at(2);
                spans.extend([
                    Span::styled(format!("{}  ", padded(before, &widths)), text_style),
                    Span::styled(format!("{:w$}", after[0], w = widths[2]), time_style),
                ]);
                let rest = match gauged {
                    true => {
                        let cell = format!("{:w$}", after[1], w = widths[GAUGE]);
                        let gauge_style = gauge.map_or(text_style, |g| g.style.patch(text_style));
                        spans.extend([
                            Span::styled("  ", text_style),
                            Span::styled(cell, gauge_style),
                        ]);
                        GAUGE + 1
                    }
                    false => GAUGE,
                };
                spans.push(Span::styled(
                    format!("  {}", aligned(&cells[rest..], &widths[rest..])),
                    text_style,
                ));
                rows.push(Row {
                    line: Line::from(spans),
                    target: Some(target),
                });
                target += 1;
            }
            panels.push(Panel {
                title: match view.herds.get(i) {
                    Some(herd) => self.outlines.title(herd, self.theme),
                    None => Line::styled(format!(" {name} "), Modifier::BOLD),
                },
                rows,
            });
        }
        let strays = stray_lines(view);
        let mut strays = strays.lines();
        if let Some(title) = strays.next() {
            let rows = strays
                .zip(target..)
                .map(|(line, target)| Row {
                    line: Line::raw(format!(" {}", line.trim_start())),
                    target: Some(target),
                })
                .collect();
            panels.push(Panel {
                title: Line::styled(format!(" {title} "), Modifier::BOLD),
                rows,
            });
        }
        panels
    }
}

/// The newest whole notes that fit in `room` lines, oldest dropped first. When
/// not even the newest fits, its opening lines, so no note shows from mid-text.
fn newest_whole(notes: Vec<Vec<Line<'static>>>, room: usize) -> Vec<Line<'static>> {
    let mut kept: Vec<Vec<Line>> = vec![];
    let mut used = 0;
    for note in notes.into_iter().rev() {
        if used + note.len() > room {
            if kept.is_empty() {
                kept.push(note.into_iter().take(room).collect());
            }
            break;
        }
        used += note.len();
        kept.push(note);
    }
    kept.into_iter().rev().flatten().collect()
}

/// `note` indented one column and broken into lines of `width` columns.
fn wrapped(note: &str, width: u16) -> Vec<Line<'static>> {
    let width = (width as usize).max(1);
    let mut lines = vec![];
    let (mut line, mut used) = (String::new(), 0);
    for c in note.chars() {
        let w = Span::raw(c.to_string()).width();
        if used + w > width && !line.is_empty() {
            lines.push(Line::raw(format!(" {}", std::mem::take(&mut line))));
            used = 0;
        }
        line.push(c);
        used += w;
    }
    lines.push(Line::raw(format!(" {line}")));
    lines
}

/// `line` in the selection's style, padded to fill `width`.
fn highlighted(line: Line<'static>, width: u16, style: Style) -> Line<'static> {
    let pad = (width as usize).saturating_sub(line.width());
    let mut spans: Vec<Span> = line
        .spans
        .into_iter()
        .map(|s| s.patch_style(style))
        .collect();
    spans.push(Span::styled(" ".repeat(pad), style));
    Line::from(spans)
}

/// `cells` each padded to its width, without trimming the last.
fn padded(cells: &[String], widths: &[usize]) -> String {
    let cells: Vec<String> = cells
        .iter()
        .zip(widths)
        .map(|(cell, w)| format!("{cell:w$}"))
        .collect();
    cells.join("  ")
}

/// `for 3m`, in the largest whole unit; blank when no change was seen.
fn time(since: Option<Duration>) -> String {
    let Some(d) = since else {
        return String::new();
    };
    match d.as_secs() {
        s if s < 60 => format!("for {s}s"),
        s if s < 3600 => format!("for {}m", s / 60),
        s => format!("for {}h", s / 3600),
    }
}

static STOP_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_stop_signal(signal: libc::c_int) {
    STOP_SIGNAL.store(signal, Ordering::Relaxed);
}

/// SIGTERM and SIGHUP set [`STOP_SIGNAL`] so the loop can restore the terminal.
fn catch_stop_signals() -> Result<()> {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction is valid, and the handler only stores an atomic.
        let failed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_stop_signal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut()) != 0
        };
        if failed {
            bail!("catch signal {signal}: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Runs the TUI until `q`, Ctrl+C, SIGTERM or SIGHUP, reading again on each
/// herdr event and polling as a fallback. Exits 128 + the signal's number when
/// a signal stopped it.
pub fn run<H, S, L>(
    herdr: H,
    subscriber: S,
    ledger: L,
    dirs: Dirs,
    options: Options,
    theme: Theme,
) -> Result<i32>
where
    H: Herdr + Send + 'static,
    S: Subscriber + 'static,
    L: Ledger + Send + 'static,
{
    catch_stop_signals()?;
    let (requests, responses) = spawn_reader(herdr, ledger, dirs);
    let watch = watch(subscriber);
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(
        &mut terminal,
        App::new(options, theme),
        &requests,
        &responses,
        &watch,
        terminal_key,
        &STOP_SIGNAL,
    );
    ratatui::try_restore()?;
    // A hung read keeps the reader thread blocked; exiting the process ends it.
    result
}

fn terminal_key(timeout: Duration) -> Result<Option<KeyEvent>> {
    if event::poll(timeout)?
        && let Event::Key(key) = event::read()?
        && key.kind == KeyEventKind::Press
    {
        return Ok(Some(key));
    }
    Ok(None)
}

/// Never touches herdr or the ledger itself, so keys and signals are handled
/// within a [`TICK`] however long a read takes.
fn run_loop<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    requests: &Sender<Request>,
    responses: &Receiver<Response>,
    watch: &Watch,
    mut next_key: impl FnMut(Duration) -> Result<Option<KeyEvent>>,
    stop: &AtomicI32,
) -> Result<i32>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    let send = |request: Request| {
        requests
            .send(request)
            .map_err(|_| anyhow!("herdr reader thread stopped"))
    };
    let mut shown = vec![];
    loop {
        let signal = stop.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(128 + signal);
        }
        if app.quit() {
            return Ok(0);
        }
        let now = Instant::now();
        while let Ok(response) = responses.try_recv() {
            app.apply(response, now)?;
        }
        for signal in watch.signals.try_iter() {
            match signal {
                Signal::Invalidated => app.invalidate(),
                Signal::Live(live) => app.set_live(live),
            }
        }
        let panes = app.panes();
        if panes != shown {
            watch.show(panes.clone());
            shown = panes;
        }
        if let Some(request) = app.tick(now) {
            send(request)?;
        }
        terminal.draw(|f| app.draw(f, now))?;
        let Some(key) = next_key(TICK)? else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(0);
        }
        if let Some(request) = app.press(key.code, Instant::now()) {
            send(request)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::style::{Color, Modifier};

    use super::*;
    use crate::herdr::fake::{FakeHerdr, FakeSubscriber, Which};
    use crate::herdr::{Agent, AgentStatus};
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::report::{self, Body, Entry};
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    fn entry(name: &str, herd: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: format!("w1:p{name}"),
            session_id: Some(format!("sess-{name}")),
            repo: "/nonexistent/repo".into(),
            worktree: Some(format!("/nonexistent/wt-{name}").into()),
            branch: None,
            workspace: "w1".into(),
            workspace_label: Some("main".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            released_at: None,
            herd: herd.map(str::to_string),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
        }
    }

    fn agent(name: Option<&str>, pane: &str, status: AgentStatus) -> Agent {
        Agent {
            name: name.map(str::to_string),
            pane_id: pane.into(),
            tab_id: pane.replace(":p", ":t"),
            status,
            session_id: name.map(|n| format!("sess-{n}")),
            launch_pending: false,
            cwd: Some("/src/elsewhere".into()),
            title: Some("poking around".into()),
        }
    }

    /// prs: a (working), b (blocked); ungrouped: c (idle), old (gone);
    /// retired: done-1; one stray.
    fn ledger() -> MemLedger {
        let mut retired = entry("done-1", Some("old-herd"));
        retired.retired = true;
        let mut ledger = MemLedger::default();
        for e in [
            entry("a", Some("prs")),
            entry("b", Some("prs")),
            entry("c", None),
            entry("old", None),
            retired,
        ] {
            ledger.0.insert(e.name.clone(), e);
        }
        ledger
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "main")])
            .with_raw_agent(agent(Some("a"), "w1:pa", AgentStatus::Working))
            .with_raw_agent(agent(Some("b"), "w1:pb", AgentStatus::Blocked))
            .with_raw_agent(agent(Some("c"), "w1:pc", AgentStatus::Idle))
            .with_raw_agent(agent(None, "w2:p7", AgentStatus::Idle))
    }

    fn app(herdr: &FakeHerdr, ledger: &MemLedger, now: Instant) -> App {
        themed(truecolor(), herdr, ledger, now)
    }

    fn themed(theme: Theme, herdr: &FakeHerdr, ledger: &MemLedger, now: Instant) -> App {
        let mut app = App::new(Options::default(), theme);
        let request = app.tick(now).expect("first read is due");
        app.apply(serve_quiet(request, herdr, ledger), now).unwrap();
        app
    }

    fn paused_app(pause: &str, now: Instant) -> App {
        let root = config_dir(&[("prs", "repo = \"/r\"\n")], None);
        std::fs::create_dir(root.path().join("paused")).unwrap();
        std::fs::write(root.path().join("paused/prs"), pause).unwrap();
        app_in(root.path(), &herdr(), &ledger(), now)
    }

    #[test]
    fn a_paused_herds_title_adds_a_light_orange_paused_badge_and_its_reason() {
        let now = Instant::now();
        let app = paused_app(
            r#"{"since": "2026-11-05T09:00:00Z", "reason": "freeze"}"#,
            now,
        );

        let terminal = render(&app, now);

        assert!(
            title_row(&terminal, "prs").starts_with("│╭ prs  ⏸ paused — freeze ─"),
            "{:#?}",
            lines(&terminal)
        );
        assert_eq!(at(&terminal, "⏸ paused").fg, LIGHT_ORANGE);
        assert_eq!(at(&terminal, "freeze").fg, LIGHT_ORANGE);
    }

    #[test]
    fn a_herd_paused_without_a_reason_shows_only_the_badge() {
        let now = Instant::now();
        let app = paused_app(r#"{"since": "2026-11-05T09:00:00Z"}"#, now);

        let terminal = render(&app, now);

        assert!(
            title_row(&terminal, "prs").starts_with("│╭ prs  ⏸ paused ─"),
            "{:#?}",
            lines(&terminal)
        );
    }

    const QUEUED_HERD: &str = r#"
repo = "/r"
[template.pr]
params = ["n"]
prompt = "go"
[queue]
template = "pr"
source = "true"
max = 3
"#;

    /// A config dir holding each herd file and, if given, `config.toml`.
    fn config_dir(herds: &[(&str, &str)], config: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("herds")).unwrap();
        for (name, text) in herds {
            std::fs::write(root.path().join(format!("herds/{name}.toml")), text).unwrap();
        }
        if let Some(config) = config {
            std::fs::write(root.path().join("config.toml"), config).unwrap();
        }
        root
    }

    /// The app after one read with herd files and pauses under `root`.
    fn app_in(root: &std::path::Path, herdr: &FakeHerdr, ledger: &MemLedger, now: Instant) -> App {
        let mut app = App::new(Options::default(), truecolor());
        let request = app.tick(now).unwrap();
        let dirs = Dirs {
            pauses: Pauses::under(root),
            mailboxes: quiet_mail(ledger).path().into(),
            herds: herd::Paths::under(root),
        };
        app.apply(serve(request, herdr, ledger, &dirs), now)
            .unwrap();
        app
    }

    /// The top border of the box titled `herd`.
    fn title_row(terminal: &Terminal<TestBackend>, herd: &str) -> String {
        let lines = lines(terminal);
        lines
            .iter()
            .find(|l| l.contains(&format!("╭ {herd} ")))
            .unwrap_or_else(|| panic!("no box {herd} in {lines:#?}"))
            .clone()
    }

    #[test]
    fn a_herd_title_counts_gone_members_but_not_its_lead_or_retired_ones_against_its_queue_max() {
        let mut ledger = ledger();
        let mut lead = entry("prs-lead", Some("prs"));
        lead.role = Some(crate::ledger::Role::Lead);
        let mut retired = entry("retired-1", Some("prs"));
        retired.retired = true;
        let gone = entry("gone-1", Some("prs"));
        for e in [lead, retired, gone] {
            ledger.0.insert(e.name.clone(), e);
        }
        let root = config_dir(&[("prs", QUEUED_HERD)], None);
        let now = Instant::now();

        let app = app_in(root.path(), &herdr(), &ledger, now);

        assert!(
            title_row(&render_tall(&app, now), "prs").starts_with("│╭ prs · 3/3 slots ─"),
            "{:#?}",
            lines(&render_tall(&app, now))
        );
    }

    #[test]
    fn a_herd_whose_file_cant_be_read_shows_unknown_slots_and_why() {
        let root = config_dir(&[], None);
        let now = Instant::now();

        let app = app_in(root.path(), &herdr(), &ledger(), now);

        let terminal = render_tall(&app, now);
        assert!(title_row(&terminal, "prs").starts_with("│╭ prs · ? slots ─"));
        assert!(
            notes_from(&terminal, "prs: ").contains("prs.toml"),
            "{:#?}",
            lines(&terminal)
        );
    }

    #[test]
    fn a_broken_config_toml_keeps_slots_turns_every_chip_off_and_says_so_once() {
        let herd = format!("{QUEUED_HERD}[lead]\nprompt = \"lead\"\nmerge = true\n");
        let other = "repo = \"/r\"\n[lead]\nprompt = \"lead\"\n";
        let mut ledger = ledger();
        let e = entry("o", Some("other"));
        ledger.0.insert(e.name.clone(), e);
        let root = config_dir(&[("prs", &herd), ("other", other)], Some("[lead_defaults"));
        let now = Instant::now();

        let app = app_in(root.path(), &herdr(), &ledger, now);

        let terminal = render_at(&app, now, 72, 40);
        assert!(
            title_row(&terminal, "prs").starts_with("│╭ prs · 2/3 slots  merge install"),
            "{:#?}",
            lines(&terminal)
        );
        let y = pos(&terminal, "prs · 2/3 slots").1;
        let row = &lines(&terminal)[y as usize];
        let x = row[..row.find("merge").unwrap()].chars().count() as u16;
        let merge = cell(&terminal, (x, y));
        assert!(
            merge.modifier.contains(Modifier::DIM) && merge.fg != ORANGE,
            "the herd file's merge = true is off, as the lead's hook enforces"
        );
        let problems = lines(&terminal)
            .iter()
            .filter(|l| l.starts_with("│ lead switches: "))
            .count();
        assert_eq!(problems, 1, "{:#?}", lines(&terminal));
    }

    #[test]
    fn a_long_pause_reason_leaves_the_switch_chips_in_view() {
        let herd = "repo = \"/r\"\n[lead]\nprompt = \"lead\"\n";
        let root = config_dir(&[("prs", herd)], None);
        std::fs::create_dir(root.path().join("paused")).unwrap();
        let reason = "x".repeat(80);
        let pause = format!(r#"{{"since": "2026-11-05T09:00:00Z", "reason": "{reason}"}}"#);
        std::fs::write(root.path().join("paused/prs"), pause).unwrap();
        let now = Instant::now();

        let app = app_in(root.path(), &herdr(), &ledger(), now);

        let terminal = render_at(&app, now, 120, 30);
        assert!(
            title_row(&terminal, "prs")
                .starts_with("│╭ prs  merge install rebase cards design dialogs  ⏸ paused — xxx"),
            "{:#?}",
            lines(&terminal)
        );
    }

    #[test]
    fn a_herd_with_a_lead_shows_its_switches_orange_when_on_and_dim_when_off() {
        let herd = "repo = \"/r\"\n[lead]\nprompt = \"lead\"\nmerge = true\nrebase = false\n";
        let defaults = "[lead_defaults]\ninstall = true\nrebase = true\n";
        let root = config_dir(&[("prs", herd)], Some(defaults));
        let now = Instant::now();

        let app = app_in(root.path(), &herdr(), &ledger(), now);

        let terminal = render_tall(&app, now);
        assert!(
            title_row(&terminal, "prs")
                .starts_with("│╭ prs  merge install rebase cards design dialogs ─"),
            "{:#?}",
            lines(&terminal)
        );
        let x = |name: &str| pos(&terminal, &format!(" {name} ")).0 + 1;
        let y = pos(&terminal, " merge ").1;
        let on = |name: &str| cell(&terminal, (x(name), y)).fg == ORANGE;
        let dim = |name: &str| {
            cell(&terminal, (x(name), y))
                .modifier
                .contains(Modifier::DIM)
        };
        assert!(on("merge"), "from the herd file");
        assert!(on("install"), "from lead_defaults");
        assert!(dim("rebase"), "the herd file over lead_defaults");
        assert!(
            dim("cards") && dim("design") && dim("dialogs"),
            "off when neither sets it"
        );
    }

    fn refresh(app: &mut App, herdr: &FakeHerdr, ledger: &MemLedger, now: Instant) {
        let response = serve_quiet(Request::Read(app.options), herdr, ledger);
        app.apply(response, now).unwrap();
    }

    /// A mailbox per ledger member holding only its `start`.
    fn quiet_mail(ledger: &MemLedger) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ledger.0.keys() {
            post(dir.path(), name, Body::Start);
        }
        dir
    }

    fn post(dir: &std::path::Path, name: &str, body: Body) {
        report::append(dir, name, &Entry::new(SystemTime::now(), body)).unwrap();
    }

    fn stop() -> Body {
        Body::Stop(report::Stop {
            background: report::Tally::Known(report::Background::default()),
            crons: report::Tally::Known(0),
            error: None,
            context: None,
            limit: None,
            session: None,
        })
    }

    fn question(text: &str) -> Body {
        Body::Question { text: text.into() }
    }

    fn done(text: &str) -> Body {
        Body::Done { text: text.into() }
    }

    /// Herd files for the fixtures' herds, with neither a queue nor a lead.
    fn plain_herds() -> herd::Paths {
        herd::Paths::under(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/view"
        )))
    }

    fn dirs(mailboxes: &std::path::Path) -> Dirs {
        Dirs {
            pauses: Pauses::under(std::path::Path::new("/nonexistent")),
            mailboxes: mailboxes.into(),
            herds: plain_herds(),
        }
    }

    fn nowhere() -> Dirs {
        dirs(std::path::Path::new("/nonexistent"))
    }

    fn serve_quiet(request: Request, herdr: &FakeHerdr, ledger: &MemLedger) -> Response {
        serve(request, herdr, ledger, &dirs(quiet_mail(ledger).path()))
    }

    /// The app after one read with `mailboxes`.
    fn app_with_mail(
        herdr: &FakeHerdr,
        ledger: &MemLedger,
        mailboxes: &std::path::Path,
        now: Instant,
    ) -> App {
        let mut app = App::new(Options::default(), truecolor());
        let request = app.tick(now).unwrap();
        app.apply(serve(request, herdr, ledger, &dirs(mailboxes)), now)
            .unwrap();
        app
    }

    /// The banner: the window's first row inside its border.
    fn banner(terminal: &Terminal<TestBackend>) -> String {
        text(terminal).swap_remove(1)
    }

    #[test]
    fn the_banner_counts_waiting_questions_and_blocked_members_in_bold_orange() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "c", question("which base?"));
        let terminal = render(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            banner(&terminal),
            "⚑ 1 question · 1 blocked — c question, b blocked"
        );
        for x in [2, 30] {
            let c = cell(&terminal, (x, 1));
            assert_eq!(c.fg, ORANGE, "x {x}");
            assert!(c.modifier.contains(Modifier::BOLD), "x {x}");
        }
    }

    #[test]
    fn with_herdr_unreachable_the_blocked_count_is_unknown_not_zero() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.unreachable = true;
        let now = Instant::now();
        let mail = quiet_mail(&ledger);
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(banner(&render(&app, now)), "⚑ 0 questions · ? blocked");
        post(mail.path(), "c", question("which base?"));
        let mut app = app;
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        assert_eq!(
            banner(&render(&app, now)),
            "⚑ 1 question · ? blocked — c question"
        );
    }

    #[test]
    fn with_herdrs_agent_list_malformed_the_blocked_count_is_unknown() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.malformed_agents = true;
        let now = Instant::now();
        let mail = quiet_mail(&ledger);
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(banner(&render(&app, now)), "⚑ 0 questions · ? blocked");
    }

    fn refresh_with_mail(
        app: &mut App,
        herdr: &FakeHerdr,
        ledger: &MemLedger,
        mailboxes: &std::path::Path,
        now: Instant,
    ) {
        let response = serve(Request::Read(app.options), herdr, ledger, &dirs(mailboxes));
        app.apply(response, now).unwrap();
    }

    #[test]
    fn with_nothing_waiting_the_banner_is_a_dim_nothing_waiting_on_you() {
        let ledger = ledger();
        let herdr = FakeHerdr::new(&[("w1", "main")])
            .with_raw_agent(agent(Some("a"), "w1:pa", AgentStatus::Working))
            .with_raw_agent(agent(Some("b"), "w1:pb", AgentStatus::Idle));
        let now = Instant::now();
        let mail = quiet_mail(&ledger);
        let terminal = render(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(banner(&terminal), "nothing waiting on you");
        assert!(cell(&terminal, (2, 1)).modifier.contains(Modifier::DIM));
    }

    #[test]
    fn the_banner_names_members_as_far_as_the_width_allows_then_an_ellipsis() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        for name in ["a", "c", "old"] {
            post(mail.path(), name, question("?"));
        }
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(
            banner(&render_at(&app, now, 48, 14)),
            "⚑ 3 questions · 1 blocked — a question …"
        );
        assert_eq!(
            banner(&render_at(&app, now, 72, 14)),
            "⚑ 3 questions · 1 blocked — a question, c question, old question …"
        );
        assert_eq!(
            banner(&render_at(&app, now, 80, 14)),
            "⚑ 3 questions · 1 blocked — a question, c question, old question, b blocked"
        );
    }

    #[test]
    fn the_banner_reserves_the_ellipsis_by_its_width_not_its_bytes() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        for name in ["a", "c", "old"] {
            post(mail.path(), name, question("?"));
        }
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(
            banner(&render_at(&app, now, 55, 14)),
            "⚑ 3 questions · 1 blocked — a question, c question …"
        );
        assert_eq!(
            banner(&render_at(&app, now, 54, 14)),
            "⚑ 3 questions · 1 blocked — a question …"
        );
    }

    #[test]
    fn the_banner_measures_a_wide_name_by_the_columns_it_takes() {
        let mut ledger = ledger();
        let wide = entry("日本語日本語日本語", None);
        ledger.0.insert(wide.name.clone(), wide);
        let (herdr, now) = (herdr(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "日本語日本語日本語", question("?"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(
            banner(&render_at(&app, now, 52, 14)),
            "⚑ 1 question · 1 blocked …"
        );
        // The test buffer shows a blank cell after each double-width glyph.
        let wide_fits = banner(&render_at(&app, now, 64, 14)).replace(' ', "");
        assert!(
            wide_fits.ends_with("—日本語日本語日本語question…"),
            "{wide_fits}"
        );
    }

    #[test]
    fn a_question_answered_by_a_later_start_or_followed_by_a_done_is_not_waiting() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "a", question("restarted since"));
        post(mail.path(), "a", Body::Start);
        post(mail.path(), "c", question("finished anyway"));
        post(mail.path(), "c", done("PR opened"));
        post(mail.path(), "old", question("still open"));
        post(mail.path(), "old", Body::Note { text: "fyi".into() });
        post(mail.path(), "old", stop());
        let terminal = render(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            banner(&terminal),
            "⚑ 1 question · 1 blocked — old question, b blocked"
        );
    }

    #[test]
    fn a_retired_members_mailbox_is_not_counted() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "done-1", question("left behind"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(
            banner(&render(&app, now)),
            "⚑ 0 questions · 1 blocked — b blocked"
        );
        let request = app.press(KeyCode::Char('a'), now).unwrap();
        app.apply(serve(request, &herdr, &ledger, &dirs(mail.path())), now)
            .unwrap();
        assert_eq!(
            banner(&render(&app, now)),
            "⚑ 0 questions · 1 blocked — b blocked",
            "nor when `a` lists the retired"
        );
    }

    #[test]
    fn a_missing_or_unreadable_mailbox_warns_without_hiding_the_others_counts() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "c", question("which base?"));
        std::fs::remove_file(mail.path().join("a.jsonl")).unwrap();
        let locked = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(mail.path().join("old.jsonl"), locked).unwrap();
        let terminal = render_tall(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            banner(&terminal),
            "⚑ 1 question · 1 blocked — c question, b blocked"
        );
        let shown = text(&terminal);
        assert!(
            shown.contains(&"warning: a: no mailbox".to_string()),
            "{shown:#?}"
        );
        assert!(
            shown.contains(
                &"warning: old: unreadable mailbox: Permission denied (os error 13)".to_string()
            ),
            "{shown:#?}"
        );
    }

    #[test]
    fn mailbox_warnings_take_a_line_each_and_never_push_herdrs_problem_off_screen() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.unreachable = true;
        let now = Instant::now();
        let mail = quiet_mail(&ledger);
        let mut a = std::fs::OpenOptions::new()
            .append(true)
            .open(mail.path().join("a.jsonl"))
            .unwrap();
        std::io::Write::write_all(&mut a, "not json\n".repeat(8).as_bytes()).unwrap();
        std::fs::remove_file(mail.path().join("c.jsonl")).unwrap();
        let terminal = render(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        let shown = text(&terminal);
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with("warning: herdr unreachable")),
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"warning: a: 8 bad lines in its mailbox".to_string()),
            "{shown:#?}"
        );
    }

    #[test]
    fn a_report_the_view_has_shown_is_still_printed_by_a_later_reports() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "c", question("which base?"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert!(banner(&render(&app, now)).contains("c question"));
        let filter = report::Filter {
            names: &[],
            herd: None,
            all: false,
            since: None,
            tail: None,
        };
        let printed = report::reports(&ledger, mail.path(), &filter).unwrap();
        assert!(
            printed.lines.iter().any(|l| l.contains("which base?")),
            "{:#?}",
            printed.lines
        );
    }

    fn truecolor() -> Theme {
        Theme::from_env(Some("truecolor"), false)
    }

    fn render(app: &App, now: Instant) -> Terminal<TestBackend> {
        render_at(app, now, 72, 14)
    }

    /// Tall enough to show every panel the fixtures make.
    fn render_tall(app: &App, now: Instant) -> Terminal<TestBackend> {
        render_at(app, now, 72, 30)
    }

    fn render_at(app: &App, now: Instant, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f, now)).unwrap();
        terminal
    }

    fn lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| {
                let line: String = row.iter().map(|c| c.symbol()).collect();
                line.trim_end().to_string()
            })
            .collect()
    }

    const PURPLE: Color = Color::Rgb(150, 50, 230);
    const ORANGE: Color = Color::Rgb(255, 154, 31);
    const LIGHT_ORANGE: Color = Color::Rgb(255, 192, 77);
    const GREY_VIOLET: Color = Color::Rgb(120, 110, 140);
    const SOFT_VIOLET: Color = Color::Rgb(160, 165, 190);
    const LIGHT: Color = Color::Rgb(235, 235, 242);

    /// Each row with the window's and boxes' side borders trimmed off.
    fn text(terminal: &Terminal<TestBackend>) -> Vec<String> {
        lines(terminal)
            .into_iter()
            .map(|l| l.trim_matches(['│', ' ']).to_string())
            .collect()
    }

    /// Where `needle` first appears below the banner.
    fn pos(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let lines = lines(terminal);
        let (y, line) = lines
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} in {lines:#?}"));
        let x = line[..line.find(needle).unwrap()].chars().count();
        (x as u16, y as u16)
    }

    fn cell(terminal: &Terminal<TestBackend>, (x, y): (u16, u16)) -> Cell {
        terminal.backend().buffer()[(x, y)].clone()
    }

    fn at(terminal: &Terminal<TestBackend>, needle: &str) -> Cell {
        cell(terminal, pos(terminal, needle))
    }

    /// Whether every cell of the row from `text` to the box's border is dimmed.
    fn dimmed(terminal: &Terminal<TestBackend>, text: &str) -> bool {
        let (x, y) = pos(terminal, text);
        (x..terminal.backend().buffer().area.width)
            .map(|x| cell(terminal, (x, y)))
            .take_while(|c| c.symbol() != "│")
            .filter(|c| c.symbol() != " ")
            .all(|c| c.modifier.contains(Modifier::DIM))
    }

    /// The notes from the one starting `start` down to the footer, with
    /// wrapped lines joined back up.
    fn notes_from(terminal: &Terminal<TestBackend>, start: &str) -> String {
        let lines = lines(terminal);
        let first = lines
            .iter()
            .position(|l| l.starts_with(&format!("│ {start}")))
            .unwrap_or_else(|| panic!("no note {start:?} in {lines:#?}"));
        let width = terminal.backend().buffer().area.width as usize - 3;
        let footer = lines.len() - 2;
        lines[first..footer]
            .iter()
            .map(|l| l.chars().skip(2).take(width).collect::<String>())
            .collect()
    }

    /// The note rows between the panels and the footer, borders trimmed.
    fn shown_notes(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let lines = lines(terminal);
        let footer = lines.len() - 2;
        let mut notes: Vec<String> = lines[..footer]
            .iter()
            .rev()
            .take_while(|l| l.starts_with("│ ") && !l.starts_with("│ ⚑"))
            .map(|l| l.trim_matches(['│', ' ']).to_string())
            .collect();
        notes.reverse();
        notes
    }

    /// The glyph leading member `name`'s row.
    fn glyph(terminal: &Terminal<TestBackend>, name: &str) -> Cell {
        let (x, y) = pos(terminal, &format!(" {name} "));
        cell(terminal, (x - 1, y))
    }

    /// The text of the row on the selection's orange.
    fn selected(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let y = (0..buffer.area.height)
            .find(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == ORANGE))
            .expect("a selected row");
        text(terminal).swap_remove(y as usize)
    }

    #[test]
    fn renders_the_status_core_with_gone_greyed_the_strays_count_and_the_keys() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let terminal = render_tall(&app(&herdr, &ledger, now), now);
        assert_eq!(
            lines(&terminal),
            [
                "╭ loopherd ────────────────────────────────────────────────────────────╮",
                "│ ⚑ 0 questions · 1 blocked — b blocked                                │",
                "│╭ prs ───────────────────────────────────────────────────────────────╮│",
                "││   NAME  STATUS            BRANCH  WORKTREE  PANE                   ││",
                "││ ⚠ b     blocked           -       wt-b      w1:tb/w1:pb            ││",
                "││ ⠋ a     working           -       wt-a      w1:ta/w1:pa            ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ ungrouped ─────────────────────────────────────────────────────────╮│",
                "││   NAME  STATUS            BRANCH  WORKTREE  PANE                   ││",
                "││ ○ c     idle              -       wt-c      w1:tc/w1:pc            ││",
                "││ ✕ old   gone              -       wt-old    w1:pold                ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ +1 stray (live in herdr, not in loopherd's ledger) ────────────────╮│",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│                                                                      │",
                "│ gone: no live agent matches it; `loopherd revive` can bring it back  │",
                "│╭────────────────────────────────────────────────────────────────────╮│",
                "││                                                                    ││",
                "││                                                                    ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│ q quit  ↑↓/jk select  enter focus  s strays: off  a all: off         │",
                "╰──────────────────────────────────────────────────────────────────────╯",
            ]
        );
        assert!(dimmed(&terminal, "old   gone"));
        assert!(!dimmed(&terminal, "c     idle"));
    }

    #[test]
    fn the_window_has_a_rounded_purple_border_titled_loopherd_in_bold() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let terminal = render(&app(&herdr, &ledger, now), now);
        for corner in [(0, 0), (71, 0), (0, 13), (71, 13)] {
            assert_eq!(cell(&terminal, corner).fg, PURPLE, "{corner:?}");
        }
        assert_eq!(cell(&terminal, (0, 0)).symbol(), "╭");
        assert_eq!(cell(&terminal, (71, 13)).symbol(), "╯");
        assert!(lines(&terminal)[0].starts_with("╭ loopherd ─"));
        assert!(cell(&terminal, (2, 0)).modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_title_runs_letter_by_letter_from_purple_to_orange_or_plain_bold_without_color() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let cases = [
            (Some("truecolor"), false, PURPLE, Color::Rgb(255, 80, 0)),
            (None, false, Color::Indexed(93), Color::Indexed(202)),
            (Some("truecolor"), true, Color::Reset, Color::Reset),
        ];
        for (colorterm, no_color, first, last) in cases {
            let theme = Theme::from_env(colorterm, no_color);
            let terminal = render(&themed(theme, &herdr, &ledger, now), now);
            let case = format!("{colorterm:?} {no_color:?}");
            let (l, d) = (cell(&terminal, (2, 0)), cell(&terminal, (9, 0)));
            assert_eq!((l.symbol(), d.symbol()), ("l", "d"), "{case}");
            assert_eq!((l.fg, d.fg), (first, last), "{case}");
            assert!(l.modifier.contains(Modifier::BOLD), "{case}");
            assert!(d.modifier.contains(Modifier::BOLD), "{case}");
        }
    }

    /// A ledger with only a retired entry, and herdr with only a stray.
    fn quiet() -> (FakeHerdr, MemLedger) {
        let mut retired = entry("done-1", Some("old-herd"));
        retired.retired = true;
        let mut ledger = MemLedger::default();
        ledger.0.insert(retired.name.clone(), retired);
        let herdr = FakeHerdr::new(&[("w1", "main")]).with_raw_agent(agent(
            None,
            "w2:p7",
            AgentStatus::Idle,
        ));
        (herdr, ledger)
    }

    const SHEEP: [&str; 4] = [
        " ▄█▄█▄█▄█▄",
        "▐█████████▌",
        " ▀█████████▄▄",
        "  ▌▐    ▌▐▓o▌",
    ];

    #[test]
    fn with_no_members_shown_a_sheep_grazes_over_the_herd_is_quiet() {
        let (herdr, ledger) = quiet();
        let now = Instant::now();
        let terminal = render(&app(&herdr, &ledger, now), now);
        let shown = lines(&terminal);
        let (_, top) = pos(&terminal, SHEEP[0]);
        for (i, row) in SHEEP.iter().enumerate() {
            assert!(shown[top as usize + i].contains(row), "{shown:#?}");
        }
        assert_eq!(at(&terminal, "█").fg, Color::Rgb(255, 255, 255));
        let (x, y) = pos(&terminal, "the herd is quiet");
        assert_eq!(y, top + 5, "{shown:#?}");
        assert!(x.abs_diff(72 - x - 17) <= 1, "centered: {shown:#?}");
        assert_eq!(cell(&terminal, (x, y)).fg, GREY_VIOLET);
    }

    #[test]
    fn a_shown_member_or_stray_sends_the_sheep_away() {
        let now = Instant::now();
        let (herdr, ledger) = (herdr(), ledger());
        let busy = render(&app(&herdr, &ledger, now), now);
        assert!(!text(&busy).iter().any(|l| l.contains("the herd is quiet")));
        let (herdr, ledger) = quiet();
        for key in ['a', 's'] {
            let mut app = app(&herdr, &ledger, now);
            press(&mut app, &[KeyCode::Char(key)], &herdr, &ledger);
            let shown = text(&render(&app, now));
            assert!(
                !shown.iter().any(|l| l.contains("the herd is quiet")),
                "{key}: {shown:#?}"
            );
            assert!(
                !shown.iter().any(|l| l.contains("▄█▄")),
                "{key}: {shown:#?}"
            );
        }
    }

    #[test]
    fn under_10_rows_of_body_the_sheep_leaves_only_the_line() {
        let (herdr, ledger) = quiet();
        let now = Instant::now();
        let terminal = render_at(&app(&herdr, &ledger, now), now, 72, 13);
        let shown = text(&terminal);
        assert!(shown.iter().any(|l| l == "the herd is quiet"), "{shown:#?}");
        assert!(!shown.iter().any(|l| l.contains("▄█▄")), "{shown:#?}");
    }

    #[test]
    fn each_member_row_leads_with_a_glyph_colored_by_its_status() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        herdr.set_status("c", AgentStatus::Done);
        let mut app = app(&herdr, &ledger, now);
        press(
            &mut app,
            &[KeyCode::Char('s'), KeyCode::Char('a')],
            &herdr,
            &ledger,
        );
        // Off the members, so no glyph takes the selection's colors.
        press(&mut app, &[KeyCode::Down; 10], &herdr, &ledger);
        let terminal = render_tall(&app, now);
        assert!(selected(&terminal).starts_with("w2:p7"));
        let shown = |name| {
            let c = glyph(&terminal, name);
            (
                c.symbol().to_string(),
                c.fg,
                c.modifier.contains(Modifier::DIM),
            )
        };
        assert_eq!(shown("a"), ("⠋".into(), ORANGE, false));
        assert_eq!(shown("b"), ("⚠".into(), Color::Yellow, false));
        assert_eq!(shown("c"), ("✓".into(), SOFT_VIOLET, false));
        assert_eq!(shown("old"), ("✕".into(), Color::Red, false));
        assert_eq!(shown("done-1"), ("?".into(), Color::Reset, true));
        herdr.set_status("c", AgentStatus::Idle);
        refresh(&mut app, &herdr, &ledger, now);
        let terminal = render_tall(&app, now);
        assert_eq!(glyph(&terminal, "c").symbol(), "○");
        assert_eq!(glyph(&terminal, "c").fg, SOFT_VIOLET);
    }

    #[test]
    fn the_working_spinner_advances_each_tick() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let app = app(&herdr, &ledger, now);
        let frames: Vec<String> = (0..3)
            .map(|i| {
                let c = glyph(&render(&app, now + TICK * i), "a");
                assert_eq!(c.fg, ORANGE);
                c.symbol().to_string()
            })
            .collect();
        assert_eq!(frames, ["⠋", "⠙", "⠹"]);
    }

    #[test]
    fn column_headings_are_bold_grey_violet_aligned_and_never_selected() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        let terminal = render(&app, now);
        let heading = at(&terminal, "NAME");
        assert_eq!(heading.fg, GREY_VIOLET);
        assert!(heading.modifier.contains(Modifier::BOLD));
        for (heading, cell) in [
            ("NAME", "b     blocked"),
            ("STATUS", "blocked"),
            ("BRANCH", "-       wt-b"),
            ("WORKTREE", "wt-b"),
            ("PANE", "w1:tb"),
        ] {
            assert_eq!(
                pos(&terminal, heading).0,
                pos(&terminal, cell).0,
                "{heading}"
            );
        }
        press(&mut app, &[KeyCode::Up], &herdr, &ledger);
        assert!(selected(&render(&app, now)).starts_with("⚠ b "));
        press(&mut app, &[KeyCode::Down; 2], &herdr, &ledger);
        assert!(selected(&render(&app, now)).starts_with("○ c "));
    }

    #[test]
    fn the_selected_row_is_light_on_orange_across_its_box_without_a_marker() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let terminal = render(&app(&herdr, &ledger, now), now);
        let (_, y) = pos(&terminal, "b     blocked");
        for x in 2..70 {
            assert_eq!(cell(&terminal, (x, y)).bg, ORANGE, "x {x}");
        }
        assert_eq!(at(&terminal, "b     blocked").fg, LIGHT);
        assert_eq!(cell(&terminal, (70, y)).bg, Color::Reset);
        assert!(!lines(&terminal).concat().contains('›'));
    }

    #[test]
    fn the_footer_shows_keys_in_light_orange_labels_in_grey_violet_and_on_off() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        let terminal = render(&app, now);
        for key in ["q quit", "↑↓/jk select", "enter focus", "s strays", "a all"] {
            assert_eq!(at(&terminal, key).fg, LIGHT_ORANGE, "{key}");
        }
        for label in ["quit", "select", "focus", "strays:", "all:"] {
            assert_eq!(at(&terminal, label).fg, GREY_VIOLET, "{label}");
        }
        let (x, y) = pos(&terminal, "strays: on");
        assert_eq!(cell(&terminal, (x + 8, y)).fg, ORANGE);
        let (x, y) = pos(&terminal, "all: off");
        assert!(cell(&terminal, (x + 5, y)).modifier.contains(Modifier::DIM));
    }

    #[test]
    fn colorterm_picks_truecolor_or_256_colors_and_no_color_drops_color() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let cases = [
            (Some("truecolor"), false, PURPLE, ORANGE),
            (Some("24bit"), false, PURPLE, ORANGE),
            (None, false, Color::Indexed(93), Color::Indexed(208)),
            (Some("256"), false, Color::Indexed(93), Color::Indexed(208)),
            (Some("truecolor"), true, Color::Reset, Color::Reset),
        ];
        for (colorterm, no_color, border, selection) in cases {
            let theme = Theme::from_env(colorterm, no_color);
            let terminal = render(&themed(theme, &herdr, &ledger, now), now);
            let case = format!("{colorterm:?} {no_color:?}");
            assert_eq!(cell(&terminal, (0, 0)).symbol(), "╭", "{case}");
            assert_eq!(cell(&terminal, (0, 0)).fg, border, "{case}");
            assert_eq!(at(&terminal, "b     blocked").bg, selection, "{case}");
            assert_eq!(glyph(&terminal, "c").symbol(), "○", "{case}");
        }
        let plain = render(
            &themed(Theme::from_env(None, true), &herdr, &ledger, now),
            now,
        );
        let buffer = plain.backend().buffer();
        assert!(
            buffer
                .content
                .iter()
                .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
        );
        assert!(
            at(&plain, "b     blocked")
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    /// Presses each key, serving its request synchronously as the reader would.
    fn press(app: &mut App, keys: &[KeyCode], herdr: &FakeHerdr, ledger: &MemLedger) {
        for key in keys {
            if let Some(request) = app.press(*key, Instant::now()) {
                app.apply(serve_quiet(request, herdr, ledger), Instant::now())
                    .unwrap();
            }
        }
    }

    #[test]
    fn s_lists_strays_and_a_shows_retired_entries_each_toggling_back() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(
            &mut app,
            &[KeyCode::Char('s'), KeyCode::Char('a')],
            &herdr,
            &ledger,
        );
        let terminal = render_tall(&app, now);
        assert_eq!(
            lines(&terminal)[2..21],
            [
                "│╭ old-herd (retired only) ───────────────────────────────────────────╮│",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ prs ───────────────────────────────────────────────────────────────╮│",
                "││   NAME    STATUS            BRANCH  WORKTREE   PANE                ││",
                "││ ⚠ b       blocked           -       wt-b       w1:tb/w1:pb         ││",
                "││ ⠋ a       working           -       wt-a       w1:ta/w1:pa         ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ ungrouped ─────────────────────────────────────────────────────────╮│",
                "││   NAME    STATUS            BRANCH  WORKTREE   PANE                ││",
                "││ ○ c       idle              -       wt-c       w1:tc/w1:pc         ││",
                "││ ✕ old     gone              -       wt-old     w1:pold             ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ retired ───────────────────────────────────────────────────────────╮│",
                "││   NAME    STATUS            BRANCH  WORKTREE   PANE                ││",
                "││ ? done-1  unknown           -       wt-done-1  w1:pdone-1          ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
                "│╭ +1 stray (live in herdr, not in loopherd's ledger) ────────────────╮│",
                "││ w2:p7  /src/elsewhere  poking around                               ││",
                "│╰────────────────────────────────────────────────────────────────────╯│",
            ]
        );
        assert_eq!(
            text(&terminal)[23],
            "gone: no live agent matches it; `loopherd revive` can bring it back"
        );
        assert_eq!(
            text(&terminal)[28],
            "q quit  ↑↓/jk select  enter focus  s strays: on  a all: on"
        );
        press(
            &mut app,
            &[KeyCode::Char('s'), KeyCode::Char('a')],
            &herdr,
            &ledger,
        );
        assert_eq!(
            lines(&render(&app, now)),
            lines(&render(&self::app(&herdr, &ledger, now), now))
        );
    }

    #[test]
    fn shows_time_in_status_only_for_changes_seen_while_open() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, t0);
        herdr.set_status("a", AgentStatus::Done);
        refresh(&mut app, &herdr, &ledger, t0 + Duration::from_secs(2));
        let terminal = render(&app, t0 + Duration::from_secs(182));
        assert_eq!(
            text(&terminal)[5],
            "✓ a     done     for 3m   -       wt-a      w1:ta/w1:pa"
        );
        assert_eq!(
            selected(&terminal),
            "⚠ b     blocked           -       wt-b      w1:tb/w1:pb"
        );
    }

    #[test]
    fn a_member_long_in_its_status_shows_its_time_light_orange_then_bold_red() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        herdr.set_status("a", AgentStatus::Idle);
        herdr.set_status("b", AgentStatus::Idle);
        let mut app = app(&herdr, &ledger, t0);
        herdr.set_status("a", AgentStatus::Working);
        herdr.set_status("b", AgentStatus::Blocked);
        refresh(&mut app, &herdr, &ledger, t0);
        // Off a and b, so the selection's colors don't cover theirs.
        press(&mut app, &[KeyCode::Down, KeyCode::Down], &herdr, &ledger);
        let minutes = |n: u64| t0 + Duration::from_secs(n * 60);

        // The first cell of `name`'s time in status.
        let time = |terminal: &Terminal<TestBackend>, name: &str| {
            let (x, _) = pos(terminal, "for ");
            cell(terminal, (x, pos(terminal, &format!(" {name} ")).1))
        };
        let bold_red = |c: Cell| (c.fg, c.modifier.contains(Modifier::BOLD)) == (Color::Red, true);

        let fresh = render(&app, minutes(4));
        assert_eq!(time(&fresh, "a").fg, Color::Reset);
        assert_eq!(time(&fresh, "b").fg, Color::Reset);

        let stale = render(&app, minutes(30));
        assert_eq!(time(&stale, "a").fg, LIGHT_ORANGE);
        assert!(!time(&stale, "a").modifier.contains(Modifier::BOLD));
        assert!(bold_red(time(&stale, "b")));

        assert!(bold_red(time(&render(&app, minutes(60)), "a")));
    }

    #[test]
    fn without_color_a_stale_time_is_underlined_and_an_overdue_one_bold() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        herdr.set_status("a", AgentStatus::Idle);
        let mut app = themed(Theme::from_env(None, true), &herdr, &ledger, t0);
        herdr.set_status("a", AgentStatus::Working);
        refresh(&mut app, &herdr, &ledger, t0);
        press(&mut app, &[KeyCode::Down, KeyCode::Down], &herdr, &ledger);

        let stale = render(&app, t0 + Duration::from_secs(30 * 60));
        let overdue = render(&app, t0 + Duration::from_secs(60 * 60));

        assert!(
            at(&stale, "for 30m")
                .modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(at(&overdue, "for 1h").modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn selection_follows_the_member_when_a_refresh_reorders_rows() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Down], &herdr, &ledger);
        herdr.set_status("a", AgentStatus::Blocked);
        herdr.set_status("b", AgentStatus::Working);
        refresh(&mut app, &herdr, &ledger, now);
        assert!(selected(&render(&app, now)).starts_with("⚠ a "));
    }

    #[test]
    fn j_k_and_arrows_move_the_selection_within_bounds() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        let selected = |app: &App| selected(&render(app, now));
        press(
            &mut app,
            &[KeyCode::Up, KeyCode::Char('k')],
            &herdr,
            &ledger,
        );
        assert!(selected(&app).starts_with("⚠ b "));
        press(
            &mut app,
            &[KeyCode::Char('j'), KeyCode::Down],
            &herdr,
            &ledger,
        );
        assert!(selected(&app).starts_with("○ c "));
        press(&mut app, &[KeyCode::Down; 5], &herdr, &ledger);
        assert!(selected(&app).starts_with("✕ old "));
    }

    #[test]
    fn enter_focuses_a_live_members_pane() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Down, KeyCode::Enter], &herdr, &ledger);
        assert_eq!(herdr.focused(), ["w1:pa"]);
    }

    #[test]
    fn enter_on_a_gone_member_focuses_nothing_and_says_why() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Down; 3], &herdr, &ledger);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        assert!(herdr.focused().is_empty());
        assert!(
            text(&render(&app, now))
                .contains(&"old is gone; `loopherd revive` can bring it back".to_string())
        );
    }

    #[test]
    fn enter_with_herdr_unreachable_focuses_nothing_and_keeps_running() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.unreachable = true;
        let now = Instant::now();
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        assert!(herdr.focused().is_empty());
        assert!(!app.quit());
        let terminal = render(&app, now);
        let shown = text(&terminal);
        assert!(
            selected(&terminal).starts_with("? a     unknown"),
            "{shown:?}"
        );
        assert!(
            text(&render_tall(&app, now))
                .iter()
                .any(|l| l.starts_with("╭ strays unknown (herdr unreachable) ─"))
        );
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with("warning: herdr unreachable"))
        );
        assert!(shown.contains(&"a has no live pane to focus".to_string()));
    }

    #[test]
    fn overflowing_notes_drop_the_oldest_whole_rather_than_its_start() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.malformed_agents = true;
        let now = Instant::now();
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        let terminal = render_at(&app, now, 30, 14);
        assert_eq!(
            shown_notes(&terminal),
            ["a has no live pane to focus"],
            "{:#?}",
            lines(&terminal)
        );
    }

    #[test]
    fn a_note_too_long_for_the_notes_area_is_cut_at_its_end() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.unreachable = true;
        let now = Instant::now();
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        let terminal = render_at(&app, now, 20, 6);
        assert_eq!(
            shown_notes(&terminal),
            ["a has no live pan"],
            "{:#?}",
            lines(&terminal)
        );
    }

    #[test]
    fn a_short_window_scrolls_to_keep_the_selection_in_view() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        press(&mut app, &[KeyCode::Down; 9], &herdr, &ledger);
        let mut terminal = Terminal::new(TestBackend::new(72, 8)).unwrap();
        terminal.draw(|f| app.draw(f, now)).unwrap();
        let shown = text(&terminal);
        assert_eq!(
            selected(&terminal),
            "w2:p7  /src/elsewhere  poking around",
            "{shown:#?}"
        );
        assert!(shown[0].starts_with("╭ loopherd "));
        assert_eq!(
            shown[5],
            "gone: no live agent matches it; `loopherd revive` can bring it back"
        );
        assert!(shown[6].starts_with("q quit"));
    }

    #[test]
    fn enter_on_a_listed_stray_focuses_its_pane() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        press(&mut app, &[KeyCode::Down; 9], &herdr, &ledger);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        assert_eq!(herdr.focused(), ["w2:p7"]);
    }

    #[test]
    fn only_reads_and_focus_reach_herdr_and_the_ledger_is_untouched() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let before = ledger.0.clone();
        let mut app = app(&herdr, &ledger, now);
        let every_key = [
            KeyCode::Char('s'),
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Up,
            KeyCode::Char('x'),
            KeyCode::Char('q'),
        ];
        for _ in 0..12 {
            press(&mut app, &every_key, &herdr, &ledger);
            press(&mut app, &[KeyCode::Down], &herdr, &ledger);
        }
        assert!(app.quit());
        assert_eq!(herdr.mutations(), 0);
        assert!(!herdr.focused().is_empty());
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn a_malformed_agent_list_shows_the_ledger_as_unknown_with_the_error() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.malformed_agents = true;
        let now = Instant::now();
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Enter], &herdr, &ledger);
        assert!(herdr.focused().is_empty());
        assert!(!app.quit());
        let terminal = render(&app, now);
        let shown = text(&terminal);
        assert!(
            selected(&terminal).starts_with("? a     unknown"),
            "{shown:?}"
        );
        assert!(
            text(&render_tall(&app, now))
                .iter()
                .any(|l| l.starts_with("╭ strays unknown (herdr's agent list unreadable) ─"))
        );
        assert!(
            notes_from(&terminal, "error:").starts_with(
                "error: herdr's `agent list` output is malformed, every status is unknown: "
            ),
            "{shown:?}"
        );
        assert!(shown.contains(&"a has no live pane to focus".to_string()));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    /// Runs the real loop on a test terminal with herdr reads served by the
    /// reader thread, feeding `keys` one per tick. Its exit code, or `None` if
    /// it hadn't returned within five seconds.
    fn run_scripted(herdr: FakeHerdr, keys: Vec<Option<KeyEvent>>, stop: i32) -> Option<i32> {
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let (requests, responses) = spawn_reader(herdr, ledger(), nowhere());
            let watch = watch(FakeSubscriber::default());
            let mut terminal = Terminal::new(TestBackend::new(72, 14)).unwrap();
            let mut keys = keys.into_iter();
            let stop = AtomicI32::new(stop);
            let code = run_loop(
                &mut terminal,
                App::new(Options::default(), truecolor()),
                &requests,
                &responses,
                &watch,
                |_| Ok(keys.next().flatten()),
                &stop,
            );
            let _ = done.send(code.unwrap());
        });
        finished.recv_timeout(Duration::from_secs(5)).ok()
    }

    #[test]
    fn q_quits_while_a_herdr_read_hangs() {
        let (_release, gate) = mpsc::channel();
        let mut herdr = herdr();
        herdr.agents_gate = Some(gate);
        let keys = vec![
            None,
            Some(key(KeyCode::Char('s'))),
            Some(key(KeyCode::Char('j'))),
            Some(key(KeyCode::Enter)),
            Some(key(KeyCode::Char('q'))),
        ];
        assert_eq!(run_scripted(herdr, keys, 0), Some(0));
    }

    #[test]
    fn ctrl_c_quits_while_a_herdr_read_hangs() {
        let (_release, gate) = mpsc::channel();
        let mut herdr = herdr();
        herdr.agents_gate = Some(gate);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(run_scripted(herdr, vec![None, Some(ctrl_c)], 0), Some(0));
    }

    #[test]
    fn a_stop_signal_ends_the_loop_with_128_plus_its_number() {
        let (_release, gate) = mpsc::channel();
        let mut herdr = herdr();
        herdr.agents_gate = Some(gate);
        assert_eq!(
            run_scripted(herdr, vec![], libc::SIGTERM),
            Some(128 + libc::SIGTERM)
        );
    }

    #[test]
    fn sigterm_and_sighup_set_the_stop_signal_instead_of_killing_the_process() {
        catch_stop_signals().unwrap();
        for signal in [libc::SIGHUP, libc::SIGTERM] {
            STOP_SIGNAL.store(0, Ordering::Relaxed);
            // SAFETY: the handler installed above only stores an atomic.
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            assert_eq!(STOP_SIGNAL.load(Ordering::Relaxed), signal);
        }
        STOP_SIGNAL.store(0, Ordering::Relaxed);
    }

    #[test]
    fn a_read_slower_than_the_poll_keeps_the_last_view_and_says_so() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, t0);
        assert_eq!(app.tick(t0 + Duration::from_secs(1)), None);
        let request = app.tick(t0 + POLL);
        assert_eq!(request, Some(Request::Read(Options::default())));
        assert_eq!(
            app.tick(t0 + POLL * 3),
            None,
            "one read in flight at a time"
        );
        let terminal = render(&app, t0 + POLL + Duration::from_secs(6));
        let shown = text(&terminal);
        assert!(selected(&terminal).starts_with("⚠ b     blocked"));
        assert!(shown.contains(&"herdr hasn't answered for 6s; showing the last read".to_string()));
    }

    #[test]
    fn a_read_for_options_since_toggled_is_ignored() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        let stale = serve_quiet(Request::Read(app.options), &herdr, &ledger);
        let toggled = app.press(KeyCode::Char('s'), now).unwrap();
        app.apply(stale, now).unwrap();
        assert!(
            !text(&render_tall(&app, now))
                .contains(&"w2:p7  /src/elsewhere  poking around".to_string())
        );
        app.apply(serve_quiet(toggled, &herdr, &ledger), now)
            .unwrap();
        assert!(
            text(&render_tall(&app, now))
                .contains(&"w2:p7  /src/elsewhere  poking around".to_string())
        );
    }

    #[test]
    fn says_it_is_reading_before_the_first_read_arrives() {
        let now = Instant::now();
        let shown = text(&render(&App::new(Options::default(), truecolor()), now));
        assert_eq!(shown[11], "reading herdr and the ledger…");
    }

    #[test]
    fn an_event_requests_a_read_before_the_poll_would() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, t0);
        let soon = t0 + Duration::from_millis(100);
        assert_eq!(app.tick(soon), None);
        app.invalidate();
        assert_eq!(app.tick(soon), Some(Request::Read(Options::default())));
    }

    #[test]
    fn events_during_a_read_queue_one_more_read_not_one_each() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, t0);
        let soon = t0 + Duration::from_millis(100);
        app.invalidate();
        let in_flight = app.tick(soon).unwrap();
        app.invalidate();
        app.invalidate();
        assert_eq!(app.tick(soon), None, "one read in flight at a time");
        app.apply(serve_quiet(in_flight, &herdr, &ledger), soon)
            .unwrap();
        let follow_up = app.tick(soon).expect("one read follows the events");
        app.apply(serve_quiet(follow_up, &herdr, &ledger), soon)
            .unwrap();
        assert_eq!(app.tick(soon), None);
    }

    #[test]
    fn polls_every_10s_while_subscribed_and_every_2s_otherwise() {
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, t0);
        app.set_live(true);
        assert_eq!(app.tick(t0 + Duration::from_secs(9)), None);
        let request = app.tick(t0 + Duration::from_secs(10)).unwrap();
        let t1 = t0 + Duration::from_secs(10);
        app.apply(serve_quiet(request, &herdr, &ledger), t1)
            .unwrap();
        app.set_live(false);
        assert_eq!(app.tick(t1 + Duration::from_secs(1)), None);
        assert!(app.tick(t1 + Duration::from_secs(2)).is_some());
    }

    #[test]
    fn watches_the_panes_of_live_members_and_listed_strays() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        assert_eq!(app.panes(), ["w1:pb", "w1:pa", "w1:pc"]);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        assert_eq!(app.panes(), ["w1:pb", "w1:pa", "w1:pc", "w2:p7"]);
    }

    #[test]
    fn a_read_with_herdr_trouble_polls_every_2s_even_while_subscribed() {
        let ledger = ledger();
        let mut herdr = herdr();
        herdr.malformed_agents = true;
        let t0 = Instant::now();
        let mut app = app(&herdr, &ledger, t0);
        app.set_live(true);
        assert!(app.tick(t0 + POLL).is_some());
    }

    #[test]
    fn an_unreadable_pause_still_polls_every_10s_while_subscribed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("paused")).unwrap();
        std::fs::write(root.path().join("paused/prs"), "not json").unwrap();
        let (herdr, ledger, t0) = (herdr(), ledger(), Instant::now());
        let mut app = App::new(Options::default(), truecolor());
        app.set_live(true);
        let request = app.tick(t0).unwrap();
        let response = serve(
            request,
            &herdr,
            &ledger,
            &Dirs {
                pauses: Pauses::under(root.path()),
                mailboxes: quiet_mail(&ledger).path().into(),
                herds: plain_herds(),
            },
        );
        app.apply(response, t0).unwrap();
        assert!(
            text(&render(&app, t0))
                .iter()
                .any(|l| l.starts_with("warning: prs's pause is unreadable")),
            "the pause warning shows"
        );
        assert_eq!(app.tick(t0 + POLL), None);
        assert!(app.tick(t0 + SUBSCRIBED_POLL).is_some());
    }

    #[test]
    fn a_herdr_event_reaches_the_loop_as_a_read_well_before_the_poll() {
        // A rendezvous gate: a send goes through only while a read waits on it.
        let (release, gate) = mpsc::sync_channel(0);
        let mut herdr = herdr();
        herdr.agents_gate = Some(gate);
        let fake = FakeSubscriber::default();
        let subscriber = fake.clone();
        let (quit, quitting) = mpsc::channel::<()>();
        let looping = std::thread::spawn(move || {
            let (requests, responses) = spawn_reader(herdr, ledger(), nowhere());
            let watch = watch(subscriber);
            let mut terminal = Terminal::new(TestBackend::new(72, 14)).unwrap();
            run_loop(
                &mut terminal,
                App::new(Options::default(), truecolor()),
                &requests,
                &responses,
                &watch,
                |timeout| {
                    std::thread::sleep(timeout.min(Duration::from_millis(5)));
                    Ok(quitting.try_recv().ok().map(|()| key(KeyCode::Char('q'))))
                },
                &AtomicI32::new(0),
            )
        });
        let read_within = |wait: Duration| {
            let deadline = Instant::now() + wait;
            loop {
                match release.try_send(()) {
                    Ok(()) => return true,
                    Err(mpsc::TrySendError::Full(())) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => return false,
                }
            }
        };
        let settled_by = Instant::now() + Duration::from_secs(3);
        while read_within(Duration::from_millis(300)) {
            assert!(Instant::now() < settled_by, "reads kept coming unprompted");
        }
        assert!(
            !read_within(POLL + Duration::from_millis(500)),
            "subscribed, the poll slows past 2s"
        );
        fake.push(Which::Lifecycle);
        assert!(
            read_within(Duration::from_secs(1)),
            "the event brought no read"
        );
        quit.send(()).unwrap();
        assert_eq!(looping.join().unwrap().unwrap(), 0);
    }

    /// The pane's box: its rows from its top border down, cut to its columns.
    fn pane(terminal: &Terminal<TestBackend>, title: &str) -> Vec<String> {
        let lines = lines(terminal);
        let top = format!("╭ {title} ");
        let (y, x) = lines
            .iter()
            .enumerate()
            .find_map(|(y, l)| l.find(&top).map(|x| (y, l[..x].chars().count())))
            .unwrap_or_else(|| panic!("no pane {title:?} in {lines:#?}"));
        lines[y..]
            .iter()
            .map(|l| l.chars().skip(x).collect::<String>())
            .take_while(|l| !l.starts_with('╰'))
            .collect()
    }

    /// The pane's text rows, borders trimmed.
    fn pane_text(terminal: &Terminal<TestBackend>, title: &str) -> Vec<String> {
        pane(terminal, title)[1..]
            .iter()
            .map(|l| l.trim_matches(['│', ' ']).to_string())
            .collect()
    }

    #[test]
    fn at_100_columns_the_body_splits_into_the_list_and_a_purple_pane_titled_with_the_selection() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let app = app(&herdr, &ledger, now);

        let wide = render_at(&app, now, 100, 20);
        let (x, y) = pos(&wide, "╭ b ");
        // Inside the window's border, 98 columns split 54/44.
        assert_eq!(x, 55, "{:#?}", lines(&wide));
        assert_eq!(cell(&wide, (x, y)).fg, PURPLE);
        assert!(lines(&wide)[y as usize].starts_with("│╭ prs "));

        let narrow = render_at(&app, now, 99, 20);
        assert!(
            !lines(&narrow).iter().any(|l| l.contains("╭ b ")),
            "{:#?}",
            lines(&narrow)
        );
        assert!(
            lines(&narrow)[2].ends_with("─╮│"),
            "the list fills the width"
        );
    }

    fn post_at(dir: &std::path::Path, name: &str, at: &str, body: Body) {
        let at = humantime::parse_rfc3339(at).unwrap();
        report::append(dir, name, &Entry::new(at, body)).unwrap();
    }

    /// A worktree on branch `feat/x`.
    fn on_branch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "--quiet", "-b", "feat/x"])
            .arg(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        dir
    }

    fn paused(shells: u32, subagents: u32) -> Body {
        Body::Stop(report::Stop {
            background: report::Tally::Known(report::Background {
                shells,
                subagents,
                other: 0,
            }),
            crons: report::Tally::Known(0),
            error: None,
            context: None,
            limit: None,
            session: None,
        })
    }

    #[test]
    fn the_pane_shows_status_and_time_template_role_branch_worktree_stop_and_latest_report() {
        let mut ledger = ledger();
        let worktree = on_branch();
        let b = ledger.0.get_mut("b").unwrap();
        b.template = Some("feat".into());
        b.worktree = Some(worktree.path().into());
        let (herdr, t0) = (herdr(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "b", "2026-11-16T10:00:00Z", done("older"));
        post_at(mail.path(), "b", "2026-11-16T10:05:00Z", paused(1, 2));
        post_at(
            mail.path(),
            "b",
            "2026-11-16T10:06:00Z",
            question("which base, main or next?"),
        );
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), t0);
        herdr.set_status("b", AgentStatus::Done);
        refresh_with_mail(
            &mut app,
            &herdr,
            &ledger,
            mail.path(),
            t0 + Duration::from_secs(2),
        );

        let terminal = render_at(&app, t0 + Duration::from_secs(182), 120, 24);

        let shown = pane_text(&terminal, "b");
        let worktree = format!("worktree {}", worktree.path().display());
        assert_eq!(
            shown[..9],
            [
                "done for 3m",
                "feat · worker",
                "branch feat/x",
                worktree.as_str(),
                "pane w1:tb/w1:pb",
                "stop 2026-11-16 10:05 paused 1 shell, 2 subagents",
                "",
                "question 2026-11-16 10:06",
                "which base, main or next?",
            ],
            "{shown:#?}"
        );
        assert_eq!(at(&terminal, "question 2026").fg, ORANGE);
    }

    #[test]
    fn the_report_is_colored_by_its_kind() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let kinds = [
            (done("ok"), "done", Color::Green),
            (question("?"), "question", ORANGE),
            (Body::Handoff { text: "h".into() }, "handoff", PURPLE),
            (Body::Note { text: "n".into() }, "note", GREY_VIOLET),
        ];
        for (body, kind, color) in kinds {
            let mail = quiet_mail(&ledger);
            post(mail.path(), "b", body);
            let app = app_with_mail(&herdr, &ledger, mail.path(), now);
            let terminal = render_at(&app, now, 120, 24);
            let (x, y) = pos(&terminal, &format!("{kind} 20"));
            assert!(x > 60, "{kind} is in the pane");
            assert_eq!(cell(&terminal, (x, y)).fg, color, "{kind}");
        }
    }

    fn measured(context: u64, limit: u64) -> Body {
        let Body::Stop(stop) = paused(0, 0) else {
            unreachable!()
        };
        Body::Stop(report::Stop {
            context: Some(context),
            limit: Some(limit),
            ..stop
        })
    }

    #[test]
    fn each_row_gauges_its_latest_stops_context_use_violet_then_orange_from_60_then_red_from_85() {
        let (ledger, herdr, t0) = (ledger(), herdr(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "a", measured(88_500, 150_000));
        post(mail.path(), "c", measured(90_000, 150_000));
        post(mail.path(), "old", measured(127_500, 150_000));
        post(mail.path(), "b", measured(127_500, 150_000));
        post(mail.path(), "b", paused(0, 0));
        let app = app_with_mail(&herdr, &ledger, mail.path(), t0);

        let terminal = render_at(&app, t0, 120, 30);

        let violet = at(&terminal, "▰▰▰▰▱▱ 59%");
        assert_eq!(violet.fg, GREY_VIOLET);
        let orange = at(&terminal, "▰▰▰▰▱▱ 60%");
        assert_eq!(orange.fg, LIGHT_ORANGE);
        let red = at(&terminal, "▰▰▰▰▰▱ 85%");
        assert_eq!(red.fg, Color::Red);
        assert!(red.modifier.contains(Modifier::BOLD));
        assert!(!violet.modifier.contains(Modifier::BOLD));
        let gauges = lines(&terminal)
            .iter()
            .filter(|l| l.contains('▰') || l.contains('▱'))
            .count();
        assert_eq!(gauges, 3, "{:#?}", lines(&terminal));
    }

    #[test]
    fn a_restarted_member_drops_its_last_sessions_gauge_and_a_gone_ones_is_dimmed() {
        let (ledger, herdr, t0) = (ledger(), herdr(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "a", measured(151_000, 150_000));
        post(mail.path(), "a", Body::Start);
        post(mail.path(), "old", measured(75_000, 150_000));
        let app = app_with_mail(&herdr, &ledger, mail.path(), t0);

        let terminal = render_at(&app, t0, 120, 30);

        assert!(at(&terminal, "▰▰▰▱▱▱ 50%").modifier.contains(Modifier::DIM));
        let gauges = lines(&terminal).iter().filter(|l| l.contains('▰')).count();
        assert_eq!(gauges, 1, "{:#?}", lines(&terminal));
    }

    #[test]
    fn a_member_with_an_empty_mailbox_shows_no_stop_report_or_blank_lines() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let shown = pane_text(&render_at(&app, now, 120, 24), "b");
        assert_eq!(
            shown[..4],
            [
                "blocked",
                "worker",
                "worktree /nonexistent/wt-b",
                "pane w1:tb/w1:pb"
            ],
            "{shown:#?}"
        );
        assert!(shown[4..].iter().all(String::is_empty), "{shown:#?}");
    }

    #[test]
    fn a_stop_says_finished_failed_or_what_is_unknown_without_claiming_a_pause() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let failed = Body::Stop(report::Stop {
            error: Some("rate_limit".into()),
            ..match stop() {
                Body::Stop(s) => s,
                _ => unreachable!(),
            }
        });
        let unknown = Body::Stop(report::Stop {
            background: report::Tally::Unknown(report::Unknown::Unknown),
            ..match stop() {
                Body::Stop(s) => s,
                _ => unreachable!(),
            }
        });
        for (body, line) in [
            (stop(), "finished"),
            (failed, "failed rate_limit"),
            (unknown, "background unknown"),
        ] {
            let mail = quiet_mail(&ledger);
            post_at(mail.path(), "b", "2026-11-16T10:05:00Z", body);
            let app = app_with_mail(&herdr, &ledger, mail.path(), now);
            let shown = pane_text(&render_at(&app, now, 120, 24), "b");
            assert!(
                shown.contains(&format!("stop 2026-11-16 10:05 {line}")),
                "{shown:#?}"
            );
        }
    }

    #[test]
    fn urls_in_the_report_are_underlined_and_the_words_around_them_are_not() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(
            mail.path(),
            "b",
            done("opened https://github.com/o/r/pull/7 for review"),
        );
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let terminal = render_at(&app, now, 120, 24);
        let (x, y) = pos(&terminal, "https://github.com/o/r/pull/7");
        let underlined = |x| {
            cell(&terminal, (x, y))
                .modifier
                .contains(Modifier::UNDERLINED)
        };
        assert!((x..x + 29).all(underlined));
        assert!(!underlined(x - 2), "opened");
        assert!(!underlined(x + 29), "the space after");
        assert!(!underlined(x + 30), "for");
    }

    #[test]
    fn a_long_report_wraps_at_words_and_is_cut_with_an_ellipsis_at_the_pane_bottom() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        let words = "lorem ipsum dolor sit amet ".repeat(40);
        post(mail.path(), "b", Body::Note { text: words });
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let shown = pane_text(&render_at(&app, now, 120, 20), "b");
        let text: Vec<_> = shown
            .iter()
            .skip_while(|l| !l.starts_with("note "))
            .skip(1)
            .collect();
        assert!(text.len() > 2, "{shown:#?}");
        for line in &text[..text.len() - 1] {
            assert!(
                line.ends_with(['m', 't', 'r', 'l']),
                "breaks between words: {line:?}"
            );
        }
        assert!(text.last().unwrap().ends_with('…'), "{shown:#?}");
    }

    #[test]
    fn a_selected_stray_shows_its_pane_tab_cwd_and_title() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        for _ in 0..4 {
            assert!(app.press(KeyCode::Down, now).is_none());
        }
        let shown = pane_text(&render_at(&app, now, 120, 24), "stray");
        assert_eq!(
            shown[..4],
            [
                "pane w2:p7",
                "tab w2:t7",
                "cwd /src/elsewhere",
                "title poking around"
            ],
        );
        assert!(shown[4..].iter().all(String::is_empty), "{shown:#?}");
    }

    #[test]
    fn moving_the_selection_redraws_the_pane_without_asking_herdr() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "a", done("a's work"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert!(pane_text(&render_at(&app, now, 120, 24), "b")[0] == "blocked");

        assert!(app.press(KeyCode::Down, now).is_none(), "no read, no focus");

        let shown = pane_text(&render_at(&app, now, 120, 24), "a");
        assert_eq!(shown[0], "working");
        assert!(shown.contains(&"a's work".to_string()), "{shown:#?}");
    }

    #[test]
    fn a_report_the_pane_has_shown_is_still_printed_by_a_later_reports() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "b", done("shipped it"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let shown = pane_text(&render_at(&app, now, 120, 24), "b");
        assert!(shown.contains(&"shipped it".to_string()), "{shown:#?}");
        let filter = report::Filter {
            names: &[],
            herd: None,
            all: false,
            since: None,
            tail: None,
        };
        let printed = report::reports(&ledger, mail.path(), &filter).unwrap();
        assert!(
            printed.lines.iter().any(|l| l.contains("shipped it")),
            "{:#?}",
            printed.lines
        );
    }

    #[test]
    fn a_url_in_brackets_is_underlined_without_its_closing_punctuation() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "b", done("see (https://x.y/z)."));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let terminal = render_at(&app, now, 120, 24);
        let (x, y) = pos(&terminal, "https://x.y/z");
        let underlined = |x| {
            cell(&terminal, (x, y))
                .modifier
                .contains(Modifier::UNDERLINED)
        };
        assert!((x..x + 13).all(underlined));
        assert!(!underlined(x - 1), "(");
        assert!(!underlined(x + 13), ")");
    }

    #[test]
    fn tabs_in_a_report_draw_as_spaces() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "b", done("key\tvalue"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let shown = pane_text(&render_at(&app, now, 120, 24), "b");
        assert!(shown.contains(&"key    value".to_string()), "{shown:#?}");
    }

    #[test]
    fn before_anything_is_selected_the_pane_is_an_empty_box() {
        let app = App::new(Options::default(), truecolor());
        let now = Instant::now();
        let terminal = render_at(&app, now, 120, 20);
        assert!(
            lines(&terminal)[2].contains("╭─"),
            "{:#?}",
            lines(&terminal)
        );
    }

    #[test]
    fn a_selected_retired_member_shows_its_report_and_stop() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "done-1", "2026-11-16T10:05:00Z", stop());
        post_at(
            mail.path(),
            "done-1",
            "2026-11-16T10:06:00Z",
            done("merged #120"),
        );
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let request = app.press(KeyCode::Char('a'), now).unwrap();
        app.apply(serve(request, &herdr, &ledger, &dirs(mail.path())), now)
            .unwrap();
        for _ in 0..4 {
            app.press(KeyCode::Down, now);
        }
        let shown = pane_text(&render_at(&app, now, 120, 24), "done-1");
        assert!(
            shown.contains(&"stop 2026-11-16 10:05 finished".to_string()),
            "{shown:#?}"
        );
        assert!(shown.contains(&"merged #120".to_string()), "{shown:#?}");
    }

    #[test]
    fn a_url_keeps_a_closing_paren_it_opened() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post(mail.path(), "b", done("(see https://w.org/Foo_(bar))"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let terminal = render_at(&app, now, 120, 24);
        let (x, y) = pos(&terminal, "https://w.org/Foo_(bar)");
        let underlined = |x| {
            cell(&terminal, (x, y))
                .modifier
                .contains(Modifier::UNDERLINED)
        };
        assert!((x..x + 23).all(underlined));
        assert!(!underlined(x + 23), "the outer )");
    }

    #[test]
    fn tabs_draw_as_spaces_on_every_pane_line() {
        let (mut herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut stray = agent(None, "w2:p8", AgentStatus::Idle);
        stray.title = Some("build\tlogs".into());
        herdr = herdr.with_raw_agent(stray);
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Char('s')], &herdr, &ledger);
        for _ in 0..5 {
            app.press(KeyCode::Down, now);
        }
        let shown = pane_text(&render_at(&app, now, 120, 24), "stray");
        assert!(
            shown.contains(&"title build    logs".to_string()),
            "{shown:#?}"
        );
    }

    /// The ticker's two rows, between its borders above the footer.
    fn ticker(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let shown = text(terminal);
        shown[shown.len() - 5..shown.len() - 3].to_vec()
    }

    fn render_ticker(app: &App, now: Instant) -> Terminal<TestBackend> {
        render_at(app, now, 72, 20)
    }

    #[test]
    fn the_ticker_shows_the_two_newest_events_across_mailboxes_newest_first() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", question("old"));
        post_at(
            mail.path(),
            "c",
            "2026-11-16T10:06:00Z",
            question("which base?\nmore"),
        );
        post_at(mail.path(), "a", "2026-11-16T10:07:00Z", done("shipped"));
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            ticker(&terminal),
            ["10:07 a done shipped", "10:06 c question which base?"]
        );
        assert_eq!(at(&terminal, "done shipped").fg, Color::Green);
        assert_eq!(at(&terminal, "question which").fg, ORANGE);
        assert_eq!(at(&terminal, "10:07").fg, Color::Reset);
    }

    #[test]
    fn the_ticker_colors_handoffs_purple_notes_grey_and_stops_dim() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:05:00Z",
            Body::Handoff {
                text: "over".into(),
            },
        );
        post_at(
            mail.path(),
            "c",
            "2026-11-16T10:06:00Z",
            Body::Note { text: "fyi".into() },
        );
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(at(&terminal, "handoff over").fg, PURPLE);
        assert_eq!(at(&terminal, "note fyi").fg, GREY_VIOLET);
        post_at(mail.path(), "c", "2026-11-16T10:07:00Z", stop());
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert!(
            at(&terminal, "stop finished")
                .modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn the_ticker_cuts_an_event_to_the_width_with_an_ellipsis() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:07:00Z",
            done("abcdefghijklmnopqrstuvwxyz"),
        );
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(
            ticker(&render_at(&app, now, 40, 20))[0],
            "10:07 a done abcdefghijklmnopqrst…"
        );
    }

    #[test]
    fn the_ticker_leaves_out_starts_and_paused_stops_but_shows_finished_and_failed() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        let stopped = |shells, error: Option<&str>| {
            Body::Stop(report::Stop {
                background: report::Tally::Known(report::Background {
                    shells,
                    ..Default::default()
                }),
                crons: report::Tally::Known(0),
                error: error.map(str::to_string),
                context: None,
                limit: None,
                session: None,
            })
        };
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", stopped(0, None));
        post_at(
            mail.path(),
            "c",
            "2026-11-16T10:06:00Z",
            stopped(0, Some("rate_limit")),
        );
        post_at(mail.path(), "a", "2026-11-16T10:07:00Z", stopped(1, None));
        post_at(mail.path(), "c", "2026-11-16T10:08:00Z", Body::Start);
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            ticker(&terminal),
            ["10:06 c stop failed rate_limit", "10:05 a stop finished"]
        );
    }

    #[test]
    fn the_ticker_leaves_out_retired_members() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", done("mine"));
        post_at(
            mail.path(),
            "done-1",
            "2026-11-16T10:06:00Z",
            done("left behind"),
        );
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        press_with_mail(&mut app, KeyCode::Char('a'), &herdr, &ledger, mail.path());
        assert_eq!(ticker(&render_ticker(&app, now)), ["10:05 a done mine", ""]);
    }

    fn press_with_mail(
        app: &mut App,
        key: KeyCode,
        herdr: &FakeHerdr,
        ledger: &MemLedger,
        mailboxes: &std::path::Path,
    ) {
        let now = Instant::now();
        let request = app.press(key, now).unwrap();
        app.apply(serve(request, herdr, ledger, &dirs(mailboxes)), now)
            .unwrap();
    }

    #[test]
    fn an_event_arriving_while_open_is_bold_for_one_read_then_normal() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", done("before"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let bold = |app: &App, needle| {
            at(&render_ticker(app, now), needle)
                .modifier
                .contains(Modifier::BOLD)
        };
        assert!(!bold(&app, "10:05"), "shown on opening, so not new");
        post_at(mail.path(), "c", "2026-11-16T10:06:00Z", note("arrived"));
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        assert!(bold(&app, "10:06"));
        assert!(bold(&app, "note arrived"));
        assert!(!bold(&app, "10:05"));
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        assert!(!bold(&app, "10:06"));
        assert!(!bold(&app, "note arrived"));
    }

    fn note(text: &str) -> Body {
        Body::Note { text: text.into() }
    }

    #[test]
    fn a_report_the_ticker_has_shown_is_still_printed_by_a_later_reports() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", done("shipped"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        assert_eq!(ticker(&render_ticker(&app, now))[0], "10:05 a done shipped");
        let filter = report::Filter {
            names: &[],
            herd: None,
            all: false,
            since: None,
            tail: None,
        };
        let printed = report::reports(&ledger, mail.path(), &filter).unwrap();
        assert!(
            printed.lines.iter().any(|l| l.contains("shipped")),
            "{:#?}",
            printed.lines
        );
    }

    #[test]
    fn below_16_rows_the_ticker_is_hidden() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", done("shipped"));
        let app = app_with_mail(&herdr, &ledger, mail.path(), now);
        let shows = |height| {
            lines(&render_at(&app, now, 72, height))
                .iter()
                .any(|l| l.contains("10:05 a done shipped"))
        };
        assert!(!shows(15));
        assert!(shows(16));
    }

    #[test]
    fn the_ticker_leaves_out_stops_with_unknown_background_work_or_crons() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        let unknown = report::Unknown::Unknown;
        let stopped = |background, crons| {
            Body::Stop(report::Stop {
                background,
                crons,
                error: None,
                context: None,
                limit: None,
                session: None,
            })
        };
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", done("before"));
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:06:00Z",
            stopped(report::Tally::Unknown(unknown), report::Tally::Known(0)),
        );
        post_at(
            mail.path(),
            "c",
            "2026-11-16T10:07:00Z",
            stopped(
                report::Tally::Known(report::Background::default()),
                report::Tally::Unknown(unknown),
            ),
        );
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(ticker(&terminal), ["10:05 a done before", ""]);
    }

    #[test]
    fn an_event_stamped_before_one_already_shown_is_still_bold_on_arrival() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "c", "2026-11-16T10:08:00Z", done("first in"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:07:30Z",
            note("landed late"),
        );
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        let terminal = render_ticker(&app, now);
        assert_eq!(
            ticker(&terminal),
            ["10:08 c done first in", "10:07 a note landed late"]
        );
        assert!(
            at(&terminal, "note landed")
                .modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !at(&terminal, "done first")
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn events_stamped_alike_put_the_later_written_on_top() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:05:00Z",
            note("written first"),
        );
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:05:00Z",
            note("written second"),
        );
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            ticker(&terminal),
            ["10:05 a note written second", "10:05 a note written first"]
        );
    }

    #[test]
    fn a_later_event_stamped_like_one_already_shown_is_bold_on_arrival() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", note("seen"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", note("arrived"));
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        let terminal = render_ticker(&app, now);
        assert_eq!(
            ticker(&terminal),
            ["10:05 a note arrived", "10:05 a note seen"]
        );
        assert!(
            at(&terminal, "note arrived")
                .modifier
                .contains(Modifier::BOLD)
        );
        assert!(!at(&terminal, "note seen").modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_recreated_mailboxs_new_events_are_bold_on_arrival() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", note("old"));
        let mut app = app_with_mail(&herdr, &ledger, mail.path(), now);
        std::fs::remove_file(mail.path().join("a.jsonl")).unwrap();
        post(mail.path(), "a", Body::Start);
        post_at(mail.path(), "a", "2026-11-16T11:00:00Z", note("brand new"));
        refresh_with_mail(&mut app, &herdr, &ledger, mail.path(), now);
        let terminal = render_ticker(&app, now);
        assert_eq!(ticker(&terminal)[0], "11:00 a note brand new");
        assert!(
            at(&terminal, "note brand new")
                .modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn events_stamped_alike_across_members_sort_by_member_name() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(mail.path(), "b", "2026-11-16T10:04:00Z", note("earlier"));
        post_at(mail.path(), "b", "2026-11-16T10:05:00Z", note("from b"));
        post_at(mail.path(), "a", "2026-11-16T10:05:00Z", note("from a"));
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(
            ticker(&terminal),
            ["10:05 a note from a", "10:05 b note from b"]
        );
    }

    #[test]
    fn the_ticker_shows_a_reports_first_non_blank_line() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mail = quiet_mail(&ledger);
        post_at(
            mail.path(),
            "a",
            "2026-11-16T10:05:00Z",
            done("\n  \nshipped\nmore"),
        );
        let terminal = render_ticker(&app_with_mail(&herdr, &ledger, mail.path(), now), now);
        assert_eq!(ticker(&terminal)[0], "10:05 a done shipped");
    }
}
