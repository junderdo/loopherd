//! Telling the human of each pull request merged into a lead's repo while
//! the lead runs with `merge` off (D85).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::github::{self, Merges};
use crate::herd::{self, Paths};
use crate::notify::{Kind, Waiting};
use crate::report::{self, Body};

/// The least time between two lookups, across every watch the lead arms.
const POLL_EVERY: Duration = Duration::from_secs(60);

/// How far behind a lookup the next one starts, so a merge GitHub's search
/// indexes late is still found.
const SEARCH_LAG: Duration = Duration::from_secs(15 * 60);

/// How long without a successful lookup before its merges come as one summary.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// How far behind the local clock a new window starts, for GitHub's clock.
const CLOCK_SKEW: Duration = Duration::from_secs(5);

/// How long the default branch is kept before it's asked again.
const BRANCH_EVERY: Duration = Duration::from_secs(60 * 60);

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
    #[serde(with = "rfc3339")]
    since: SystemTime,
    #[serde(with = "rfc3339")]
    polled: SystemTime,
    branch: Option<String>,
    /// When `branch` was asked; `None` asks again.
    #[serde(default, with = "rfc3339::option")]
    branched: Option<SystemTime>,
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
    /// lookup is due. A lead's first lookup only starts its window. After
    /// over [`STALE_AFTER`] without a successful one, the merges come as one
    /// summary and the window starts over. Repeats are left to
    /// `notify::send`'s marks. A failure warns once a streak.
    pub fn poll(
        &mut self,
        paths: &Paths,
        merges: &dyn Merges,
        now: SystemTime,
        warn: &mut dyn FnMut(&str),
    ) -> Vec<Waiting> {
        self.try_poll(paths, merges, now)
            .unwrap_or_else(|e| self.failed(&e, warn))
    }

    fn try_poll(
        &mut self,
        paths: &Paths,
        merges: &dyn Merges,
        now: SystemTime,
    ) -> Result<Vec<Waiting>> {
        let Some(repo) = herd::merge_alerts(paths, &self.herd)? else {
            return Ok(vec![]);
        };
        let baseline = || State {
            created: self.created.clone(),
            since: now - CLOCK_SKEW,
            polled: now,
            branch: None,
            branched: None,
        };
        let Some(mut state) = self.load()?.filter(|s| s.created == self.created) else {
            self.save(&baseline())?;
            return Ok(vec![]);
        };
        // A `polled` ahead of `now` is a clock that jumped back, so it's due.
        let since_poll = now.duration_since(state.polled).unwrap_or(POLL_EVERY);
        if since_poll < POLL_EVERY {
            return Ok(vec![]);
        }
        state.polled = now;
        // A `since` ahead of `now` was saved by a clock running ahead.
        let since = if state.since > now {
            now - SEARCH_LAG
        } else {
            state.since
        };
        let since = since.max(self.revived()?.unwrap_or(SystemTime::UNIX_EPOCH));
        let fresh = |at: SystemTime| now.duration_since(at).is_ok_and(|d| d < BRANCH_EVERY);
        let found = match (&state.branch, state.branched) {
            (Some(branch), Some(at)) if fresh(at) => Ok(branch.clone()),
            _ => merges
                .default_branch(&repo)
                .inspect(|_| state.branched = Some(now)),
        }
        .and_then(|branch| {
            state.branch = Some(branch.clone());
            merges.merged_since(&repo, &branch, &seconds(since))
        });
        let prs = match found {
            Ok(prs) => prs,
            Err(e) => {
                self.save(&state)?;
                return Err(e);
            }
        };
        self.failing = false;
        if now
            .duration_since(since)
            .is_ok_and(|gap| gap > SEARCH_LAG + STALE_AFTER)
        {
            self.save(&State {
                branch: state.branch,
                branched: state.branched,
                ..baseline()
            })?;
            return Ok(self.summary(prs.len(), now).into_iter().collect());
        }
        state.since = since.max(now - SEARCH_LAG);
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

    /// One line for `count` merges found at `now`, or none for none.
    fn summary(&self, count: usize, now: SystemTime) -> Option<Waiting> {
        let text = match count {
            0 => return None,
            1 => "1 merge while unwatched".to_string(),
            n if n >= github::MERGED_LIMIT => format!("{n} or more merges while unwatched"),
            n => format!("{n} merges while unwatched"),
        };
        Some(Waiting {
            herd: Some(self.herd.clone()),
            member: self.lead.clone(),
            kind: Kind::MergeAlert,
            text,
            at: Some(format!("unwatched {}", seconds(now))),
        })
    }

    fn failed(&mut self, e: &anyhow::Error, warn: &mut dyn FnMut(&str)) -> Vec<Waiting> {
        if !self.failing {
            warn(&format!("{}'s merges: {e:#}", self.herd));
        }
        self.failing = true;
        vec![]
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

    /// `None` when there is none or it can't be parsed, which starts a new window.
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

/// A time kept as RFC 3339 whole seconds.
mod rfc3339 {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(at: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::seconds(*at))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let text = String::deserialize(d)?;
        humantime::parse_rfc3339_weak(&text).map_err(D::Error::custom)
    }

    pub mod option {
        use std::time::SystemTime;

        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(at: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
            match at {
                Some(at) => super::serialize(at, s),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<SystemTime>, D::Error> {
            #[derive(Deserialize)]
            struct Wrap(#[serde(with = "super")] SystemTime);
            Ok(Option::<Wrap>::deserialize(d)?.map(|Wrap(at)| at))
        }
    }
}
