//! Telling the human of each pull request merged into a lead's repo while
//! the lead runs with `merge` off (D85).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::github::Merges;
use crate::herd::{self, Paths};
use crate::notify::{Kind, Waiting};
use crate::report::{self, Body};

/// The least time between two lookups, across every watch the lead arms.
const POLL_EVERY: Duration = Duration::from_secs(60);

/// How far behind a lookup the next one starts, so a merge GitHub's search
/// indexes late is still found.
const SEARCH_LAG: Duration = Duration::from_secs(15 * 60);

/// One lead's merge lookups, from the lead's watch.
pub struct MergeAlerts {
    herd: String,
    lead: String,
    /// The lead's ledger `created`, telling its state from a predecessor's.
    created: String,
    mailboxes: PathBuf,
    /// Whether the last lookup failed, so a streak warns once.
    failing: bool,
}

/// What `<lead>.merges` keeps between watches.
#[derive(Serialize, Deserialize)]
struct State {
    created: String,
    /// The earliest merge time the next lookup asks for.
    since: String,
    polled: String,
    branch: Option<String>,
}

impl MergeAlerts {
    pub fn new(herd: &str, lead: &str, created: &str, mailboxes: &Path) -> MergeAlerts {
        MergeAlerts {
            herd: herd.into(),
            lead: lead.into(),
            created: created.into(),
            mailboxes: mailboxes.into(),
            failing: false,
        }
    }

    /// The merges to alert at `now`, when the herd file asks for them and a
    /// lookup is due. A lead's first lookup only sets the start of its
    /// window to `now`. Repeats are left to `notify::send`'s marks.
    pub fn poll(
        &mut self,
        paths: &Paths,
        merges: &dyn Merges,
        now: SystemTime,
        warn: &mut dyn FnMut(&str),
    ) -> Result<Vec<Waiting>> {
        let repo = match herd::merge_alerts(paths, &self.herd) {
            Ok(Some(repo)) => repo,
            Ok(None) => return Ok(vec![]),
            Err(e) => return self.failed(&e, warn),
        };
        let Some(mut state) = self.load()?.filter(|s| s.created == self.created) else {
            self.save(&State {
                created: self.created.clone(),
                since: seconds(now),
                polled: seconds(now),
                branch: None,
            })?;
            return Ok(vec![]);
        };
        if parse(&state.polled)? + POLL_EVERY > now {
            return Ok(vec![]);
        }
        state.polled = seconds(now);
        let since = parse(&state.since)?.max(self.revived()?.unwrap_or(SystemTime::UNIX_EPOCH));
        let found = match &state.branch {
            Some(branch) => Ok(branch.clone()),
            None => merges.default_branch(&repo),
        }
        .and_then(|branch| {
            state.branch = Some(branch.clone());
            merges.merged_since(&repo, &branch, &seconds(since))
        });
        let prs = match found {
            Ok(prs) => prs,
            Err(e) => {
                self.save(&state)?;
                return self.failed(&e, warn);
            }
        };
        self.failing = false;
        state.since = seconds(since.max(now - SEARCH_LAG));
        self.save(&state)?;
        Ok(prs
            .into_iter()
            .map(|pr| Waiting {
                herd: Some(self.herd.clone()),
                member: self.lead.clone(),
                kind: Kind::MergeAlert,
                text: format!("#{} {}", pr.number, pr.url),
                at: Some(pr.url),
            })
            .collect())
    }

    fn failed(&mut self, e: &anyhow::Error, warn: &mut dyn FnMut(&str)) -> Result<Vec<Waiting>> {
        if !self.failing {
            warn(&format!("{}'s merges: {e:#}", self.herd));
        }
        self.failing = true;
        Ok(vec![])
    }

    /// When this lead last came back: the first `start` after its last
    /// `gone`, ignoring a predecessor's entries.
    fn revived(&self) -> Result<Option<SystemTime>> {
        let created = parse(&self.created)?;
        let (entries, _) = report::history(&self.mailboxes, &self.lead)?;
        let mut revived = None;
        let mut down = false;
        for entry in entries {
            if parse(&entry.at)? < created {
                continue;
            }
            match entry.body {
                Body::Gone { .. } => down = true,
                Body::Start { .. } if down => {
                    revived = Some(parse(&entry.at)?);
                    down = false;
                }
                _ => {}
            }
        }
        Ok(revived)
    }

    fn path(&self) -> PathBuf {
        self.mailboxes.join(format!("{}.merges", self.lead))
    }

    /// `None` when there is none or it can't be read, which starts a new window.
    fn load(&self) -> Result<Option<State>> {
        let path = self.path();
        match fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text).ok()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        let path = self.path();
        let text = serde_json::to_string(state)?;
        fs::create_dir_all(&self.mailboxes)
            .with_context(|| format!("create {}", self.mailboxes.display()))?;
        // Renamed into place, so a watch killed mid-write leaves the old state.
        let new = path.with_extension("merges.new");
        fs::write(&new, text).with_context(|| format!("write {}", new.display()))?;
        fs::rename(&new, &path).with_context(|| format!("write {}", path.display()))
    }
}

fn parse(at: &str) -> Result<SystemTime> {
    humantime::parse_rfc3339_weak(at).with_context(|| format!("read time {at}"))
}

/// In whole seconds, as GitHub's search reads it.
fn seconds(at: SystemTime) -> String {
    humantime::format_rfc3339_seconds(at).to_string()
}
