//! `loopherd view`: the herd view, live.

mod since;
mod watcher;

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::herd_view::{Member, Options, Stray};
use crate::herdr::Herdr;
use crate::herdr::events::Subscriber;
use crate::ledger::Ledger;
use crate::status::{Read, aligned, read, row, sections, stray_lines, widths};
use since::Since;
use watcher::{Signal, Watch, watch};

const POLL: Duration = Duration::from_secs(2);
/// The ledger and mailbox push no events, so a read still comes this often
/// while herdr's events cover agent status.
const SUBSCRIBED_POLL: Duration = Duration::from_secs(10);
/// How long the loop waits for a key before checking replies and signals.
const TICK: Duration = Duration::from_millis(100);
/// Wide enough for `for 59m`, so the columns don't jump when a time appears.
const TIME_WIDTH: usize = 7;

/// Work for the reader thread: anything that touches herdr or the ledger,
/// which can hang, so the input loop never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Read(Options),
    Focus(String),
}

pub enum Response {
    Read(Options, Result<Read>),
    Focus(String, Result<()>),
}

pub fn serve(request: Request, herdr: &dyn Herdr, ledger: &dyn Ledger) -> Response {
    match request {
        Request::Read(options) => Response::Read(options, read(herdr, ledger, options)),
        Request::Focus(pane) => {
            let result = herdr.focus(&pane);
            Response::Focus(pane, result)
        }
    }
}

/// Serves requests in order on its own thread until the sender drops.
fn spawn_reader<H, L>(herdr: H, ledger: L) -> (Sender<Request>, Receiver<Response>)
where
    H: Herdr + Send + 'static,
    L: Ledger + Send + 'static,
{
    let (requests, inbox) = mpsc::channel();
    let (outbox, responses) = mpsc::channel();
    std::thread::spawn(move || {
        for request in inbox {
            if outbox.send(serve(request, &herdr, &ledger)).is_err() {
                break;
            }
        }
    });
    (requests, responses)
}

pub struct App {
    options: Options,
    read: Option<Read>,
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
    text: String,
    dim: bool,
    /// Index among the selectable rows.
    target: Option<usize>,
}

impl App {
    pub fn new(options: Options) -> App {
        App {
            options,
            read: None,
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
    /// see herdr's panes to subscribe to.
    fn poll(&self) -> Duration {
        let herdr_read = self.read.as_ref().is_some_and(|r| r.problem.is_none());
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
                let read = result?;
                self.since = self.since.observe(&read.view, now);
                self.read = Some(read);
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
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
        let rows = self.rows(now);
        let selected_line = rows
            .iter()
            .position(|r| r.target == Some(self.selected))
            .unwrap_or(0);
        let scroll = (selected_line + 1).saturating_sub(body.height as usize);
        let lines: Vec<Line> = rows
            .into_iter()
            .map(|r| {
                let text = match r.target {
                    Some(i) if i == self.selected => format!("› {}", r.text),
                    Some(_) => format!("  {}", r.text),
                    None => r.text,
                };
                match r.dim {
                    true => Line::styled(text, Style::new().add_modifier(Modifier::DIM)),
                    false => Line::raw(text),
                }
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            body,
        );
        frame.render_widget(Line::raw(self.keys()), footer);
    }

    fn keys(&self) -> String {
        let on = |b: bool| if b { "on" } else { "off" };
        format!(
            "q quit  ↑↓/jk select  enter focus  s strays: {}  a all: {}",
            on(self.options.strays),
            on(self.options.all)
        )
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

    fn rows(&self, now: Instant) -> Vec<Row> {
        let mut rows = self.view_rows(now);
        let plain = |text: String| Row {
            text,
            dim: false,
            target: None,
        };
        match (&self.read, self.reading) {
            (None, _) => rows.push(plain("reading herdr and the ledger…".into())),
            (Some(_), Some(since)) if now.saturating_duration_since(since) >= POLL => {
                let waited = now.saturating_duration_since(since).as_secs();
                rows.push(plain(format!(
                    "herdr hasn't answered for {waited}s; showing the last read"
                )));
            }
            _ => {}
        }
        rows.extend(self.message.clone().map(plain));
        rows
    }

    fn view_rows(&self, now: Instant) -> Vec<Row> {
        let Some(read) = &self.read else {
            return vec![];
        };
        let view = &read.view;
        let sections = sections(view);
        let cells: Vec<Vec<Vec<String>>> = sections
            .iter()
            .map(|(_, members)| {
                members
                    .iter()
                    .map(|m| {
                        let mut cells = row(m);
                        cells.insert(2, time(self.since.of(&m.name, now)));
                        cells
                    })
                    .collect()
            })
            .collect();
        let mut widths = widths(cells.iter().flatten());
        if let Some(w) = widths.get_mut(2) {
            *w = (*w).max(TIME_WIDTH);
        }
        let plain = |text: String| Row {
            text,
            dim: false,
            target: None,
        };
        let mut rows = vec![];
        let mut target = 0;
        for ((name, members), cells) in sections.iter().zip(&cells) {
            rows.push(plain(name.clone()));
            for (member, cells) in members.iter().zip(cells) {
                rows.push(Row {
                    text: aligned(cells, &widths),
                    dim: member.gone,
                    target: Some(target),
                });
                target += 1;
            }
        }
        let strays = stray_lines(view);
        let mut strays = strays.lines();
        rows.extend(strays.next().map(|l| plain(l.into())));
        for line in strays {
            rows.push(Row {
                text: line.trim_start().into(),
                dim: false,
                target: Some(target),
            });
            target += 1;
        }
        if sections.iter().any(|(_, m)| m.iter().any(|m| m.gone)) {
            rows.push(plain(
                "gone: no live agent matches it; `loopherd revive` can bring it back".into(),
            ));
        }
        rows.extend(read.problem.clone().map(plain));
        rows
    }
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
pub fn run<H, S, L>(herdr: H, subscriber: S, ledger: L, options: Options) -> Result<i32>
where
    H: Herdr + Send + 'static,
    S: Subscriber + 'static,
    L: Ledger + Send + 'static,
{
    catch_stop_signals()?;
    let (requests, responses) = spawn_reader(herdr, ledger);
    let watch = watch(subscriber);
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(
        &mut terminal,
        App::new(options),
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
    use ratatui::style::Modifier;

    use super::*;
    use crate::herdr::fake::{FakeHerdr, FakeSubscriber, Which};
    use crate::herdr::{Agent, AgentStatus};
    use crate::ledger::{AgentRecord, MemLedger};

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
            herd: herd.map(str::to_string),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
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
        let mut app = App::new(Options::default());
        let request = app.tick(now).expect("first read is due");
        app.apply(serve(request, herdr, ledger), now).unwrap();
        app
    }

    fn refresh(app: &mut App, herdr: &FakeHerdr, ledger: &MemLedger, now: Instant) {
        let response = serve(Request::Read(app.options), herdr, ledger);
        app.apply(response, now).unwrap();
    }

    fn render(app: &App, now: Instant) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(72, 14)).unwrap();
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

    /// Whether every cell of the row starting with `text` is dimmed.
    fn dimmed(terminal: &Terminal<TestBackend>, text: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let y = lines(terminal)
            .iter()
            .position(|l| l.trim_start_matches(['›', ' ']).starts_with(text))
            .unwrap_or_else(|| panic!("no row {text:?}"));
        let row = &buffer.content[y * buffer.area.width as usize..][..buffer.area.width as usize];
        row.iter()
            .filter(|c| c.symbol() != " ")
            .all(|c| c.modifier.contains(Modifier::DIM))
    }

    #[test]
    fn renders_the_status_core_with_gone_greyed_the_strays_count_and_the_keys() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let terminal = render(&app(&herdr, &ledger, now), now);
        assert_eq!(
            lines(&terminal),
            [
                "prs",
                "› b    blocked           -  wt-b    w1:tb/w1:pb",
                "  a    working           -  wt-a    w1:ta/w1:pa",
                "ungrouped",
                "  c    idle              -  wt-c    w1:tc/w1:pc",
                "  old  gone              -  wt-old  w1:pold",
                "+1 stray (live in herdr, not in loopherd's ledger)",
                "gone: no live agent matches it; `loopherd revive` can bring it back",
                "",
                "",
                "",
                "",
                "",
                "q quit  ↑↓/jk select  enter focus  s strays: off  a all: off",
            ]
        );
        assert!(dimmed(&terminal, "old"));
        assert!(!dimmed(&terminal, "c "));
    }

    /// Presses each key, serving its request synchronously as the reader would.
    fn press(app: &mut App, keys: &[KeyCode], herdr: &FakeHerdr, ledger: &MemLedger) {
        for key in keys {
            if let Some(request) = app.press(*key, Instant::now()) {
                app.apply(serve(request, herdr, ledger), Instant::now())
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
        let shown = lines(&render(&app, now));
        assert_eq!(
            shown[..12],
            [
                "old-herd (retired only)",
                "prs",
                "› b       blocked           -  wt-b       w1:tb/w1:pb",
                "  a       working           -  wt-a       w1:ta/w1:pa",
                "ungrouped",
                "  c       idle              -  wt-c       w1:tc/w1:pc",
                "  old     gone              -  wt-old     w1:pold",
                "retired",
                "  done-1  unknown           -  wt-done-1  w1:pdone-1",
                "+1 stray (live in herdr, not in loopherd's ledger)",
                "  w2:p7  /src/elsewhere  poking around",
                "gone: no live agent matches it; `loopherd revive` can bring it back",
            ]
        );
        assert_eq!(
            shown[13],
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
        let shown = lines(&render(&app, t0 + Duration::from_secs(182)));
        assert_eq!(shown[2], "  a    done     for 3m   -  wt-a    w1:ta/w1:pa");
        assert_eq!(shown[1], "› b    blocked           -  wt-b    w1:tb/w1:pb");
    }

    #[test]
    fn selection_follows_the_member_when_a_refresh_reorders_rows() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        press(&mut app, &[KeyCode::Down], &herdr, &ledger);
        herdr.set_status("a", AgentStatus::Blocked);
        herdr.set_status("b", AgentStatus::Working);
        refresh(&mut app, &herdr, &ledger, now);
        assert!(lines(&render(&app, now))[1].starts_with("› a "));
    }

    #[test]
    fn j_k_and_arrows_move_the_selection_within_bounds() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        let selected = |app: &App| {
            lines(&render(app, now))
                .into_iter()
                .find(|l| l.starts_with('›'))
                .unwrap()
        };
        press(
            &mut app,
            &[KeyCode::Up, KeyCode::Char('k')],
            &herdr,
            &ledger,
        );
        assert!(selected(&app).starts_with("› b "));
        press(
            &mut app,
            &[KeyCode::Char('j'), KeyCode::Down],
            &herdr,
            &ledger,
        );
        assert!(selected(&app).starts_with("› c "));
        press(&mut app, &[KeyCode::Down; 5], &herdr, &ledger);
        assert!(selected(&app).starts_with("› old "));
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
            lines(&render(&app, now))
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
        let shown = lines(&render(&app, now));
        assert!(shown[1].starts_with("› a    unknown"), "{shown:?}");
        assert!(shown.contains(&"strays unknown (herdr unreachable)".to_string()));
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with("warning: herdr unreachable"))
        );
        assert!(shown.contains(&"a has no live pane to focus".to_string()));
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
        let shown = lines(&render(&app, now));
        assert!(shown[1].starts_with("› a    unknown"), "{shown:?}");
        assert!(shown.contains(&"strays unknown (herdr's agent list unreadable)".to_string()));
        assert!(
            shown.iter().any(|l| l.starts_with(
                "error: herdr's `agent list` output is malformed, every status is unknown"
            )),
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
            let (requests, responses) = spawn_reader(herdr, ledger());
            let watch = watch(FakeSubscriber::default());
            let mut terminal = Terminal::new(TestBackend::new(72, 14)).unwrap();
            let mut keys = keys.into_iter();
            let stop = AtomicI32::new(stop);
            let code = run_loop(
                &mut terminal,
                App::new(Options::default()),
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
        let shown = lines(&render(&app, t0 + POLL + Duration::from_secs(6)));
        assert!(shown[1].starts_with("› b    blocked"));
        assert!(shown.contains(&"herdr hasn't answered for 6s; showing the last read".to_string()));
    }

    #[test]
    fn a_read_for_options_since_toggled_is_ignored() {
        let (herdr, ledger, now) = (herdr(), ledger(), Instant::now());
        let mut app = app(&herdr, &ledger, now);
        let stale = serve(Request::Read(app.options), &herdr, &ledger);
        let toggled = app.press(KeyCode::Char('s'), now).unwrap();
        app.apply(stale, now).unwrap();
        assert!(
            !lines(&render(&app, now))
                .contains(&"  w2:p7  /src/elsewhere  poking around".to_string())
        );
        app.apply(serve(toggled, &herdr, &ledger), now).unwrap();
        assert!(
            lines(&render(&app, now))
                .contains(&"  w2:p7  /src/elsewhere  poking around".to_string())
        );
    }

    #[test]
    fn says_it_is_reading_before_the_first_read_arrives() {
        let now = Instant::now();
        let shown = lines(&render(&App::new(Options::default()), now));
        assert_eq!(shown[0], "reading herdr and the ledger…");
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
        app.apply(serve(in_flight, &herdr, &ledger), soon).unwrap();
        let follow_up = app.tick(soon).expect("one read follows the events");
        app.apply(serve(follow_up, &herdr, &ledger), soon).unwrap();
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
        app.apply(serve(request, &herdr, &ledger), t1).unwrap();
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
    fn a_herdr_event_reaches_the_loop_as_a_read_well_before_the_poll() {
        // A rendezvous gate: a send goes through only while a read waits on it.
        let (release, gate) = mpsc::sync_channel(0);
        let mut herdr = herdr();
        herdr.agents_gate = Some(gate);
        let fake = FakeSubscriber::default();
        let subscriber = fake.clone();
        let (quit, quitting) = mpsc::channel::<()>();
        let looping = std::thread::spawn(move || {
            let (requests, responses) = spawn_reader(herdr, ledger());
            let watch = watch(subscriber);
            let mut terminal = Terminal::new(TestBackend::new(72, 14)).unwrap();
            run_loop(
                &mut terminal,
                App::new(Options::default()),
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
        while read_within(Duration::from_millis(300)) {}
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
}
