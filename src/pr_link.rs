//! `loopherd pr`: herdr's link handler for a Ctrl-clicked GitHub PR. The
//! member that owns the PR shows in a popup; any other PR opens in the
//! browser.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Result, anyhow};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind};

use crate::herd_view::member as member_view;
use crate::herdr::Herdr;
use crate::ledger::AgentRecord;
use crate::report::{self, Entry, Kind};
use crate::watch::printable;

/// The plugin's popup pane entrypoint, declared in `herdr-plugin.toml`.
pub const POPUP: &str = "pr-member";

const LAST_REPORTS: usize = 5;

/// What a click did.
#[derive(Debug, PartialEq, Eq)]
pub enum Shown {
    Popup { member: String },
    Browser,
}

/// Which branch a pull request is from.
pub trait PrHeads {
    /// `None` when GitHub has no such PR.
    fn head_of(&self, pr: &str) -> Result<Option<String>>;
}

pub fn click(
    url: &str,
    entries: &BTreeMap<String, AgentRecord>,
    reports_dir: &Path,
    herdr: &dyn Herdr,
    heads: &dyn PrHeads,
    browser: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<Shown> {
    let Some(pr) = pr_url(url) else {
        browser(url)?;
        return Ok(Shown::Browser);
    };
    let owner = match named_in_reports(pr, entries, reports_dir)? {
        Some(name) => Some(name),
        None => on_head_branch(pr, entries, heads),
    };
    if let Some(member) = owner {
        let env = [
            ("LOOPHERD_PR_MEMBER", member.as_str()),
            ("LOOPHERD_PR_URL", pr),
        ];
        match herdr.open_plugin_pane(POPUP, &env) {
            Ok(()) => return Ok(Shown::Popup { member }),
            Err(e) => eprintln!("loopherd pr: no popup for {member}, opening {pr} instead: {e:#}"),
        }
    }
    browser(pr)?;
    Ok(Shown::Browser)
}

/// `https://github.com/<owner>/<repo>/pull/<n>` at the start of `url`.
fn pr_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(4, '/');
    let (owner, repo, pull) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = parts.next()?;
    let n = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if owner.is_empty() || repo.is_empty() || pull != "pull" || n == 0 {
        return None;
    }
    let len = "https://github.com/".len() + owner.len() + repo.len() + "/pull/".len() + 1 + n;
    Some(&url[..len])
}

/// The open member whose current run reported `pr` last.
fn named_in_reports(
    pr: &str,
    entries: &BTreeMap<String, AgentRecord>,
    reports_dir: &Path,
) -> Result<Option<String>> {
    let mut latest: Option<(SystemTime, &str)> = None;
    for record in entries.values().filter(|r| !r.retired) {
        let (history, _) = report::history(reports_dir, &record.name)?;
        let naming = current_run(&history, record)
            .filter(|e| e.report().is_some_and(|(_, text)| names(text, pr)))
            .filter_map(|e| humantime::parse_rfc3339_weak(&e.at).ok())
            .max();
        if let Some(at) = naming
            && latest.is_none_or(|(best, _)| at > best)
        {
            latest = Some((at, &record.name));
        }
    }
    Ok(latest.map(|(_, name)| name.to_string()))
}

/// The entries since `record` was spawned; a name fed again keeps its mailbox.
fn current_run<'a>(
    history: &'a [Entry],
    record: &AgentRecord,
) -> impl Iterator<Item = &'a Entry> + use<'a> {
    let since = humantime::parse_rfc3339_weak(&record.created).ok();
    history.iter().filter(
        move |e| match (since, humantime::parse_rfc3339_weak(&e.at)) {
            (Some(since), Ok(at)) => at >= since,
            _ => true,
        },
    )
}

/// The open member whose recorded branch is `pr`'s head; `None` when GitHub can't say.
fn on_head_branch(
    pr: &str,
    entries: &BTreeMap<String, AgentRecord>,
    heads: &dyn PrHeads,
) -> Option<String> {
    let head = match heads.head_of(pr) {
        Ok(head) => head?,
        Err(e) => {
            eprintln!("loopherd pr: can't ask GitHub for {pr}'s branch: {e:#}");
            return None;
        }
    };
    entries
        .values()
        .find(|r| !r.retired && r.branch.as_deref() == Some(head.as_str()))
        .map(|r| r.name.clone())
}

/// The popup's text for `member`, the owner of `pr`.
pub fn card(
    member: &str,
    pr: &str,
    entries: &BTreeMap<String, AgentRecord>,
    reports_dir: &Path,
    herdr: &dyn Herdr,
) -> Result<String> {
    let record = entries
        .get(member)
        .ok_or_else(|| anyhow!("{member} is not in the ledger"))?;
    let status = match herdr.agents() {
        Ok(agents) => {
            let live = member_view(record, Some(&agents), |_| None);
            match live.gone {
                true => "gone".to_string(),
                false => live.status.as_str().to_string(),
            }
        }
        Err(_) => "unknown (herdr unreachable)".to_string(),
    };
    let (history, _) = report::history(reports_dir, member)?;
    let reports: Vec<(&Entry, Kind, &str)> = current_run(&history, record)
        .filter_map(|e| e.report().map(|(kind, text)| (e, kind, text)))
        .collect();
    let rounds = reports.iter().filter(|(_, k, _)| *k == Kind::Done).count();
    let review = match rounds {
        0 => "no done report yet".to_string(),
        n => format!("round {n}"),
    };
    let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let mut text = format!(
        "{pr}\n\nmember    {member}\nherd      {}\nstatus    {status}\ntemplate  {}\nreview    {review}\n\nlast reports\n",
        dash(&record.herd),
        dash(&record.template),
    );
    let last = &reports[reports.len().saturating_sub(LAST_REPORTS)..];
    for (entry, kind, said) in last {
        let said = printable(&said.split_whitespace().collect::<Vec<_>>().join(" "));
        text.push_str(&format!("{}  {}  {said}\n", entry.when(), kind.as_str()));
    }
    if last.is_empty() {
        text.push_str("none\n");
    }
    text.push_str("\nq or Esc to close\n");
    Ok(text)
}

/// Reads `next` until the human presses q or Escape.
pub fn wait_to_close(next: &mut dyn FnMut() -> std::io::Result<Event>) -> Result<()> {
    loop {
        if let Event::Key(key) = next()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

/// `text` holds `url` not running on into a longer PR number.
fn names(text: &str, url: &str) -> bool {
    text.match_indices(url)
        .any(|(i, _)| !text[i + url.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::report::Body;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    use std::time::Duration;

    const PR: &str = "https://github.com/o/r/pull/12";

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("abc".into()),
            repo: "/src/repo".into(),
            worktree: None,
            branch: Some(format!("feat/{name}")),
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            released_at: None,
            herd: Some("loopherd".into()),
            template: Some("impl".into()),
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            earlier: vec![],
        }
    }

    fn entries(records: impl IntoIterator<Item = AgentRecord>) -> BTreeMap<String, AgentRecord> {
        records.into_iter().map(|r| (r.name.clone(), r)).collect()
    }

    fn at(minutes: u64) -> SystemTime {
        humantime::parse_rfc3339("2026-11-01T00:00:00Z").unwrap()
            + Duration::from_secs(minutes * 60)
    }

    fn done(dir: &Path, name: &str, minutes: u64, text: &str) {
        let body = Body::Done { text: text.into() };
        report::append(dir, name, &Entry::new(at(minutes), body)).unwrap();
    }

    struct Clicked {
        shown: Shown,
        herdr: FakeHerdr,
        opened: Vec<String>,
    }

    /// GitHub's answer to which branch a PR is from.
    struct Heads(Result<Option<&'static str>, &'static str>);

    impl PrHeads for Heads {
        fn head_of(&self, pr: &str) -> Result<Option<String>> {
            assert_eq!(pr, PR, "asked about another PR");
            match &self.0 {
                Ok(head) => Ok(head.map(str::to_string)),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    fn click_on(url: &str, records: Vec<AgentRecord>, dir: &Path) -> Clicked {
        click_with(url, records, dir, Heads(Ok(None)), FakeHerdr::new(&[]))
    }

    fn click_with(
        url: &str,
        records: Vec<AgentRecord>,
        dir: &Path,
        heads: Heads,
        herdr: FakeHerdr,
    ) -> Clicked {
        let mut opened = vec![];
        let shown = click(url, &entries(records), dir, &herdr, &heads, &mut |url| {
            opened.push(url.to_string());
            Ok(())
        })
        .unwrap();
        Clicked {
            shown,
            herdr,
            opened,
        }
    }

    #[test]
    fn a_member_whose_report_names_the_pr_gets_the_popup() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 5, &format!("{PR} adds the thing"));

        let c = click_on(PR, vec![record("lh-1"), record("lh-2")], dir.path());

        assert_eq!(
            c.shown,
            Shown::Popup {
                member: "lh-1".into()
            }
        );
        assert_eq!(
            c.herdr.popups(),
            [vec![
                ("LOOPHERD_PR_MEMBER".to_string(), "lh-1".to_string()),
                ("LOOPHERD_PR_URL".to_string(), PR.to_string()),
            ]]
        );
        assert!(c.opened.is_empty());
    }

    fn retired(name: &str) -> AgentRecord {
        AgentRecord {
            retired: true,
            retired_at: Some("2026-11-02T00:00:00Z".into()),
            ..record(name)
        }
    }

    fn assert_browser(c: &Clicked, url: &str) {
        assert_eq!(c.shown, Shown::Browser);
        assert_eq!(c.opened, [url]);
        assert!(c.herdr.popups().is_empty());
    }

    #[test]
    fn a_pr_no_open_member_reported_opens_in_the_browser() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 5, &format!("{PR}3 is a longer number"));
        done(dir.path(), "lh-2", 5, &format!("{PR} merged"));

        let c = click_on(PR, vec![record("lh-1"), retired("lh-2")], dir.path());

        assert_browser(&c, PR);
    }

    #[test]
    fn a_name_fed_again_owns_only_what_its_current_run_reported() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 5, &format!("{PR} from the first run"));
        let refed = AgentRecord {
            created: "2026-11-01T01:00:00Z".into(),
            earlier: vec![retired("lh-1")],
            ..record("lh-1")
        };

        let c = click_on(PR, vec![refed], dir.path());

        assert_browser(&c, PR);
    }

    #[test]
    fn the_member_that_reported_the_pr_last_owns_it() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 9, &format!("verified {PR}"));
        done(dir.path(), "lh-2", 5, &format!("{PR} opened"));

        let c = click_on(PR, vec![record("lh-1"), record("lh-2")], dir.path());

        assert_eq!(
            c.shown,
            Shown::Popup {
                member: "lh-1".into()
            }
        );
    }

    #[test]
    fn a_member_on_the_prs_head_branch_gets_the_popup() {
        let dir = tempfile::tempdir().unwrap();

        let c = click_with(
            PR,
            vec![record("lh-1"), record("lh-2")],
            dir.path(),
            Heads(Ok(Some("feat/lh-2"))),
            FakeHerdr::new(&[]),
        );

        assert_eq!(
            c.shown,
            Shown::Popup {
                member: "lh-2".into()
            }
        );
    }

    #[test]
    fn a_retired_member_on_the_head_branch_leaves_it_to_the_browser() {
        let dir = tempfile::tempdir().unwrap();

        let c = click_with(
            PR,
            vec![retired("lh-2")],
            dir.path(),
            Heads(Ok(Some("feat/lh-2"))),
            FakeHerdr::new(&[]),
        );

        assert_browser(&c, PR);
    }

    #[test]
    fn a_pr_github_cant_be_asked_about_opens_in_the_browser() {
        let dir = tempfile::tempdir().unwrap();

        let c = click_with(
            PR,
            vec![record("lh-1")],
            dir.path(),
            Heads(Err("gh: not logged in")),
            FakeHerdr::new(&[]),
        );

        assert_browser(&c, PR);
    }

    #[test]
    fn a_link_into_a_prs_page_finds_the_pr_and_names_it_in_the_popup() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 5, PR);
        let link = format!("{PR}/files#diff-1");

        let c = click_on(&link, vec![record("lh-1")], dir.path());

        assert_eq!(
            c.shown,
            Shown::Popup {
                member: "lh-1".into()
            }
        );
        assert!(c.herdr.popups()[0].contains(&("LOOPHERD_PR_URL".into(), PR.into())));
    }

    #[test]
    fn a_link_that_isnt_a_pr_opens_as_clicked() {
        let dir = tempfile::tempdir().unwrap();
        let link = "https://github.com/o/r/pulls";

        let c = click_on(link, vec![record("lh-1")], dir.path());

        assert_browser(&c, link);
    }

    #[test]
    fn a_refused_popup_falls_back_to_the_browser() {
        let dir = tempfile::tempdir().unwrap();
        done(dir.path(), "lh-1", 5, PR);
        let herdr = FakeHerdr::new(&[]);
        herdr.refuse_popups();

        let c = click_with(PR, vec![record("lh-1")], dir.path(), Heads(Ok(None)), herdr);

        assert_browser(&c, PR);
    }

    fn note(dir: &Path, name: &str, minutes: u64, text: &str) {
        let body = Body::Note { text: text.into() };
        report::append(dir, name, &Entry::new(at(minutes), body)).unwrap();
    }

    #[test]
    fn the_card_shows_the_member_its_status_round_and_last_reports() {
        let dir = tempfile::tempdir().unwrap();
        for (minute, text) in [(1, "one"), (2, "two"), (3, "three"), (4, "four")] {
            note(dir.path(), "lh-1", minute, text);
        }
        done(dir.path(), "lh-1", 5, &format!("{PR} first round"));
        done(dir.path(), "lh-1", 9, "fixed the review\nsecond line");
        let start = Entry::new(at(10), Body::Start { resumed: true });
        report::append(dir.path(), "lh-1", &start).unwrap();
        let herdr = FakeHerdr::new(&[]).with_raw_agent(crate::herdr::Agent {
            name: Some("lh-1".into()),
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            status: crate::herdr::AgentStatus::Working,
            session_id: Some("abc".into()),
            launch_pending: false,
            cwd: None,
            title: None,
            display_agent: None,
        });

        let text = card("lh-1", PR, &entries([record("lh-1")]), dir.path(), &herdr).unwrap();

        assert_eq!(
            text,
            format!(
                "{PR}\n\
                 \n\
                 member    lh-1\n\
                 herd      loopherd\n\
                 status    working\n\
                 template  impl\n\
                 review    round 2\n\
                 \n\
                 last reports\n\
                 2026-11-01 00:02  note  two\n\
                 2026-11-01 00:03  note  three\n\
                 2026-11-01 00:04  note  four\n\
                 2026-11-01 00:05  done  {PR} first round\n\
                 2026-11-01 00:09  done  fixed the review second line\n\
                 \n\
                 q or Esc to close\n"
            )
        );
    }

    #[test]
    fn the_card_says_what_it_cant_know() {
        let dir = tempfile::tempdir().unwrap();
        let bare = AgentRecord {
            herd: None,
            template: None,
            ..record("lh-1")
        };
        let herdr = FakeHerdr::new(&[]);
        herdr.set_unreachable(true);

        let text = card("lh-1", PR, &entries([bare]), dir.path(), &herdr).unwrap();

        assert_eq!(
            text,
            format!(
                "{PR}\n\
                 \n\
                 member    lh-1\n\
                 herd      -\n\
                 status    unknown (herdr unreachable)\n\
                 template  -\n\
                 review    no done report yet\n\
                 \n\
                 last reports\n\
                 none\n\
                 \n\
                 q or Esc to close\n"
            )
        );
    }

    #[test]
    fn a_member_no_longer_in_herdr_shows_as_gone() {
        let dir = tempfile::tempdir().unwrap();

        let text = card(
            "lh-1",
            PR,
            &entries([record("lh-1")]),
            dir.path(),
            &FakeHerdr::new(&[]),
        )
        .unwrap();

        assert!(text.contains("\nstatus    gone\n"), "{text}");
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn the_popup_waits_for_q_or_escape() {
        for close in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut events = vec![
                key(KeyCode::Char('x')),
                key(KeyCode::Enter),
                Event::Resize(80, 24),
                key(close),
                key(KeyCode::Char('z')),
            ]
            .into_iter();

            wait_to_close(&mut || Ok(events.next().unwrap())).unwrap();

            assert_eq!(events.next(), Some(key(KeyCode::Char('z'))), "{close:?}");
        }
    }
}
