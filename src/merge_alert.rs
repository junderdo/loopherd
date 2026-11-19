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
use crate::notify::{self, Kind, Waiting};
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

/// How long wakes may hold a due lookup off before it runs on a waking round.
const WAKES_HOLD_OFF: Duration = Duration::from_secs(15 * 60);

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
    /// This watch's last lookup, which holds the next off when the window
    /// that records it couldn't be saved.
    looked: Option<SystemTime>,
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
            looked: None,
        }
    }

    /// The merges to alert at `now`, when the herd file asks for them and a
    /// lookup is due. A lead's first lookup only starts its window. After
    /// over [`STALE_AFTER`] without a successful one, the merges not yet sent
    /// come as one summary. On a `waking` round a lookup waits, up to
    /// [`WAKES_HOLD_OFF`]. Repeats are left to `notify::send`'s marks. A
    /// failure warns once a streak.
    pub fn poll(
        &mut self,
        paths: &Paths,
        merges: &dyn Merges,
        now: SystemTime,
        waking: bool,
        warn: &mut dyn FnMut(&str),
    ) -> Vec<Waiting> {
        match self.try_poll(paths, merges, now, waking) {
            Ok(Some(found)) => {
                self.failing = false;
                found
            }
            Ok(None) => vec![],
            Err(e) => self.failed(&e, warn),
        }
    }

    /// `None` when no lookup ran and was saved.
    fn try_poll(
        &mut self,
        paths: &Paths,
        merges: &dyn Merges,
        now: SystemTime,
        waking: bool,
    ) -> Result<Option<Vec<Waiting>>> {
        let Some(repo) = herd::merge_alerts(paths, &self.herd)? else {
            return Ok(None);
        };
        let Some(mut state) = self.load()?.filter(|s| s.created == self.created) else {
            self.save(&State {
                created: self.created.clone(),
                since: now - CLOCK_SKEW,
                polled: now,
                branch: None,
                branched: None,
            })?;
            return Ok(None);
        };
        // A `polled` ahead of `now` is a clock that jumped back, so it's due.
        let since_poll = now.duration_since(state.polled).unwrap_or(POLL_EVERY);
        let since_look = self.looked.and_then(|at| now.duration_since(at).ok());
        let since_poll = since_look.map_or(since_poll, |look| look.min(since_poll));
        if since_poll < POLL_EVERY || (waking && since_poll < WAKES_HOLD_OFF) {
            return Ok(None);
        }
        state.polled = now;
        self.looked = Some(now);
        let window = state.since;
        let revived = self.revived()?.filter(|at| *at <= now);
        let since = state.since.max(revived.unwrap_or(SystemTime::UNIX_EPOCH));
        // Ahead of `now`, it was saved by a clock running ahead.
        let since = if since > now { now - SEARCH_LAG } else { since };
        let fresh = |at: SystemTime| now.duration_since(at).is_ok_and(|d| d < BRANCH_EVERY);
        let found = match (&state.branch, state.branched) {
            (Some(branch), Some(at)) if fresh(at) => Ok(branch.clone()),
            (cached, _) => merges
                .default_branch(&repo)
                .inspect(|_| state.branched = Some(now))
                .or_else(|e| cached.clone().ok_or(e)),
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
        let stale = now
            .duration_since(since)
            .is_ok_and(|gap| gap > SEARCH_LAG + STALE_AFTER);
        let total = prs.len();
        state.since = since.max(now - SEARCH_LAG);
        self.save(&state)?;
        if !stale {
            return Ok(Some(
                prs.into_iter()
                    .map(|pr| self.alert(format!("#{} {}", pr.number, pr.url), pr.url))
                    .collect(),
            ));
        }
        // Marked only once the window is saved, so an unsaved one finds them again
        // unmarked, and under one lock, so two watches don't each count a part.
        let urls: Vec<&str> = prs.iter().map(|pr| pr.url.as_str()).collect();
        let marked =
            notify::first_to_notify_each(&self.mailboxes, &self.lead, Kind::MergeAlert, &urls);
        let unsent = match marked {
            Ok(unsent) => unsent,
            Err(e) => {
                // The summary couldn't be marked either, so the next lookup counts them
                // again. If this save fails too, the window stays moved and they're missed.
                state.since = window;
                self.save(&state)?;
                return Err(e.context("mark merges sent"));
            }
        };
        let text = match unsent {
            0 => return Ok(Some(vec![])),
            n if total >= github::MERGED_LIMIT => format!("{n} or more merges while unwatched"),
            1 => "1 merge while unwatched".to_string(),
            n => format!("{n} merges while unwatched"),
        };
        let at = format!("unwatched {}", seconds(now));
        Ok(Some(vec![self.alert(text, at)]))
    }

    fn alert(&self, text: String, at: String) -> Waiting {
        Waiting {
            herd: Some(self.herd.clone()),
            member: self.lead.clone(),
            kind: Kind::MergeAlert,
            text,
            at: Some(at),
        }
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::github::Pr;
    use crate::github::fake::FakeMerges;

    const CREATED: &str = "2026-11-01T08:00:00Z";
    const FIRST: &str = "2026-11-01T09:00:00Z";

    /// A herd `cards` whose lead's merges are alerted, with its mailboxes.
    struct World {
        dir: tempfile::TempDir,
        paths: Paths,
    }

    impl World {
        fn new() -> World {
            let dir = tempfile::tempdir().unwrap();
            let herds = dir.path().join("herds");
            fs::create_dir(&herds).unwrap();
            let herd = "repo = \"/repo\"\n[lead]\nprompt = \"go\"\n";
            fs::write(herds.join("cards.toml"), herd).unwrap();
            let paths = Paths {
                herds,
                home: PathBuf::from("/home/me"),
                feed_lock: dir.path().join("feed.lock"),
                paused: dir.path().join("paused"),
            };
            World { dir, paths }
        }

        fn mailboxes(&self) -> PathBuf {
            self.dir.path().join("reports")
        }

        fn watch(&self) -> MergeAlerts {
            MergeAlerts::new("cards", "lead-cards", CREATED, &self.mailboxes())
        }

        /// What one watch's lookup at `at` sends and warns.
        fn poll(&self, merges: &dyn Merges, at: &str) -> (Vec<String>, Vec<String>) {
            let mut warned = vec![];
            let sent =
                self.watch()
                    .poll(&self.paths, merges, parse(at).unwrap(), false, &mut |w| {
                        warned.push(w.to_string())
                    });
            (sent.into_iter().map(|w| w.text).collect(), warned)
        }
    }

    fn gh(merged: Vec<(&'static str, u64)>) -> FakeMerges {
        FakeMerges::new(Path::new("/repo"), merged)
    }

    /// Holds each merge lookup until every watch has reached its own.
    struct Together(FakeMerges, Arc<Barrier>);

    impl Merges for Together {
        fn default_branch(&self, repo: &Path) -> Result<String> {
            self.0.default_branch(repo)
        }

        fn merged_since(&self, repo: &Path, branch: &str, since: &str) -> Result<Vec<Pr>> {
            let prs = self.0.merged_since(repo, branch, since);
            self.1.wait();
            prs
        }
    }

    /// Enough tries that marks taken under a lock each fail: about 1 try in
    /// 250 split them between the watches.
    const RACES: usize = 1000;

    #[test]
    fn two_watches_looking_up_at_once_send_one_stale_summary_between_them() {
        for _ in 0..RACES {
            let world = World::new();
            world.poll(&gh(vec![]), FIRST);
            let barrier = Arc::new(Barrier::new(2));

            let sent: Vec<String> = thread::scope(|s| {
                let watches = ["2026-11-01T12:00:00Z"; 2].map(|at| {
                    let (world, barrier) = (&world, barrier.clone());
                    s.spawn(move || {
                        let merged = (1..=50).map(|n| ("2026-11-01T10:00:00Z", n)).collect();
                        let gh = Together(gh(merged), barrier);
                        world.poll(&gh, at).0
                    })
                });
                watches
                    .into_iter()
                    .flat_map(|w| w.join().unwrap())
                    .collect()
            });

            assert_eq!(sent, ["50 merges while unwatched"]);
        }
    }

    #[test]
    fn a_stale_summary_that_cannot_be_marked_warns_and_is_sent_once_it_can() {
        let world = World::new();
        world.poll(&gh(vec![]), FIRST);
        let blocker = world.mailboxes().join("lead-cards.notified");
        fs::create_dir(&blocker).unwrap();
        let gh = gh(vec![("2026-11-01T10:00:00Z", 7)]);

        let (sent, warned) = world.poll(&gh, "2026-11-01T12:00:00Z");

        assert_eq!(sent, Vec::<String>::new());
        assert_eq!(warned.len(), 1, "{warned:?}");
        let mark = "cards's merges: mark merges sent: ";
        assert!(warned[0].starts_with(mark), "{warned:?}");
        assert!(warned[0].contains("lead-cards.notified"), "{warned:?}");

        fs::remove_dir(&blocker).unwrap();
        let (sent, warned) = world.poll(&gh, "2026-11-01T12:01:00Z");

        assert_eq!(sent, ["1 merge while unwatched"]);
        assert_eq!(warned, Vec::<String>::new());
    }

    #[test]
    fn a_stale_lookup_finding_no_merges_leaves_the_sent_marks_alone() {
        let world = World::new();
        world.poll(&gh(vec![]), FIRST);

        let (sent, warned) = world.poll(&gh(vec![]), "2026-11-01T12:00:00Z");

        assert_eq!((sent, warned), (vec![], vec![]));
        assert!(!world.mailboxes().join("lead-cards.notified").exists());
    }
}
