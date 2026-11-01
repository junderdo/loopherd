//! `loopherd view`: the herd view, live.

mod since;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::herd_view::{Member, Options, Stray};
use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::status::{Read, aligned, read, row, sections, stray_lines, widths};
use since::Since;

const POLL: Duration = Duration::from_secs(2);
/// Wide enough for `for 59m`, so the columns don't jump when a time appears.
const TIME_WIDTH: usize = 7;

pub struct App {
    options: Options,
    read: Option<Read>,
    since: Since,
    selected: usize,
    /// The selected row's identity, to keep it selected as rows re-sort.
    selected_key: Option<Key>,
    message: Option<String>,
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
            quit: false,
        }
    }

    /// Re-reads herdr and the ledger through `status`'s code path.
    pub fn refresh(&mut self, herdr: &dyn Herdr, ledger: &dyn Ledger, now: Instant) -> Result<()> {
        let read = read(herdr, ledger, self.options)?;
        self.since = self.since.observe(&read.view, now);
        self.read = Some(read);
        let kept = self
            .selected_key
            .as_ref()
            .and_then(|key| self.keys_in_order().iter().position(|k| k == key));
        self.select(kept.unwrap_or(self.selected));
        Ok(())
    }

    pub fn press(
        &mut self,
        key: KeyCode,
        herdr: &dyn Herdr,
        ledger: &dyn Ledger,
        now: Instant,
    ) -> Result<()> {
        self.message = None;
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('s') => {
                self.options.strays = !self.options.strays;
                self.refresh(herdr, ledger, now)?;
            }
            KeyCode::Char('a') => {
                self.options.all = !self.options.all;
                self.refresh(herdr, ledger, now)?;
            }
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Enter => self.message = self.focus(herdr),
            _ => {}
        }
        Ok(())
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

    /// Focuses the selected agent's pane; the reason it didn't, if it didn't.
    fn focus(&self, herdr: &dyn Herdr) -> Option<String> {
        let targets = self.targets();
        let pane = match targets.get(self.selected)? {
            Target::Member(m) if m.gone => {
                return Some(format!(
                    "{} is gone; `loopherd revive` can bring it back",
                    m.name
                ));
            }
            // Only a live agent has a tab.
            Target::Member(m) if m.tab.is_none() => {
                return Some(format!("{} has no live pane to focus", m.name));
            }
            Target::Member(m) => &m.pane,
            Target::Stray(s) => &s.pane,
        };
        herdr
            .focus(pane)
            .err()
            .map(|e| format!("focus {pane}: {e:#}"))
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
        rows.extend(self.message.clone().map(plain));
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

/// Runs the TUI until `q`, polling herdr every two seconds.
pub fn run(herdr: &dyn Herdr, ledger: &dyn Ledger, options: Options) -> Result<i32> {
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(&mut terminal, herdr, ledger, options);
    ratatui::try_restore()?;
    result.map(|()| 0)
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    options: Options,
) -> Result<()> {
    let mut app = App::new(options);
    app.refresh(herdr, ledger, Instant::now())?;
    let mut next = Instant::now() + POLL;
    while !app.quit() {
        terminal.draw(|f| app.draw(f, Instant::now()))?;
        if event::poll(next.saturating_duration_since(Instant::now()))? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }
                app.press(key.code, herdr, ledger, Instant::now())?;
            }
        } else {
            app.refresh(herdr, ledger, Instant::now())?;
            next = Instant::now() + POLL;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::{Agent, AgentStatus};
    use crate::ledger::{AgentRecord, MemLedger};

    fn entry(name: &str, herd: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: format!("w1:p{name}"),
            session_id: Some(format!("sess-{name}")),
            repo: "/nonexistent/repo".into(),
            worktree: Some(format!("/nonexistent/wt-{name}").into()),
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
        app.refresh(herdr, ledger, now).unwrap();
        app
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

    fn press(app: &mut App, keys: &[KeyCode], herdr: &FakeHerdr, ledger: &MemLedger) {
        for key in keys {
            app.press(*key, herdr, ledger, Instant::now()).unwrap();
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
        app.refresh(&herdr, &ledger, t0 + Duration::from_secs(2))
            .unwrap();
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
        app.refresh(&herdr, &ledger, now).unwrap();
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
}
