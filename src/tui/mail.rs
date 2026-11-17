//! The members' mailboxes as the view reads them: through `report::history`,
//! with no cursor and no lock, so the view never consumes a report that
//! `loopherd reports` or the watch would print.

use std::collections::BTreeMap;
use std::path::Path;

use crate::herd_view::{Member, View};
use crate::herdr::AgentStatus;
use crate::report::{Body, Entry, history};

#[derive(Debug, Default)]
pub struct Mail {
    boxes: BTreeMap<String, Vec<Entry>>,
    /// One short line per mailbox that is missing, unreadable or has bad lines.
    pub warnings: Vec<String>,
}

impl Mail {
    /// The mailboxes of `view`'s members. Retired ones, listed only with
    /// `all`, are read for the detail pane and never warned about.
    pub fn read(dir: &Path, view: &View) -> Mail {
        let mut mail = Mail::default();
        for member in view.retired.iter().flatten() {
            if let Ok((entries, _)) = history(dir, &member.name) {
                mail.boxes.insert(member.name.clone(), entries);
            }
        }
        for member in active(view) {
            let name = &member.name;
            if !dir.join(format!("{name}.jsonl")).is_file() {
                mail.warnings.push(format!("warning: {name}: no mailbox"));
                continue;
            }
            match history(dir, name) {
                Ok((entries, bad)) => {
                    let lines = if bad.len() == 1 { "line" } else { "lines" };
                    if !bad.is_empty() {
                        mail.warnings.push(format!(
                            "warning: {name}: {} bad {lines} in its mailbox",
                            bad.len()
                        ));
                    }
                    mail.boxes.insert(name.clone(), entries);
                }
                Err(e) => mail.warnings.push(format!(
                    "warning: {name}: unreadable mailbox: {}",
                    e.root_cause()
                )),
            }
        }
        mail
    }

    /// Its newest report and its newest stop.
    pub fn latest(&self, name: &str) -> (Option<&Entry>, Option<&Entry>) {
        let entries = self.boxes.get(name).map_or(&[][..], Vec::as_slice);
        let newest = |is: fn(&Entry) -> bool| entries.iter().rev().find(|e| is(e));
        (
            newest(|e| e.report().is_some()),
            newest(|e| matches!(e.body, Body::Stop(_))),
        )
    }

    /// The non-retired members' reports and stops, newest first, less
    /// stops that left work in flight or unknown.
    pub fn events<'a>(&'a self, view: &'a View) -> Vec<(&'a str, &'a Entry)> {
        let mut events: Vec<_> = active(view)
            .flat_map(|m| {
                let entries = self.boxes.get(&m.name).map_or(&[][..], Vec::as_slice);
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (m.name.as_str(), i, e))
            })
            .filter(|(_, _, e)| match &e.body {
                Body::Start => false,
                Body::Stop(s) => s.error.is_some() || s.in_flight() == (None, None),
                _ => true,
            })
            .collect();
        events.sort_by(|(_, i, a), (_, j, b)| (&b.at, j).cmp(&(&a.at, i)));
        events.into_iter().map(|(name, _, e)| (name, e)).collect()
    }

    /// Its latest question has no `start` or `done` after it.
    fn asking(&self, name: &str) -> bool {
        let entries = self.boxes.get(name).map_or(&[][..], Vec::as_slice);
        for entry in entries.iter().rev() {
            match entry.body {
                Body::Question { .. } => return true,
                Body::Done { .. } | Body::Start => return false,
                _ => {}
            }
        }
        false
    }
}

/// What waits on the human: questions, then blocked members, each in
/// display order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Waiting {
    pub questions: usize,
    pub blocked: usize,
    /// `<name> question` or `<name> blocked`, one per thing waiting.
    pub items: Vec<String>,
}

pub fn waiting(view: &View, mail: &Mail) -> Waiting {
    let asking: Vec<_> = active(view)
        .filter(|m| mail.asking(&m.name))
        .map(|m| format!("{} question", m.name))
        .collect();
    let blocked: Vec<_> = active(view)
        .filter(|m| m.status == AgentStatus::Blocked)
        .map(|m| format!("{} blocked", m.name))
        .collect();
    Waiting {
        questions: asking.len(),
        blocked: blocked.len(),
        items: [asking, blocked].concat(),
    }
}

/// Members of the herds, then `ungrouped`, as the view shows them; never
/// the retired.
fn active(view: &View) -> impl Iterator<Item = &Member> {
    view.herds
        .iter()
        .flat_map(|h| &h.members)
        .chain(&view.ungrouped)
}
