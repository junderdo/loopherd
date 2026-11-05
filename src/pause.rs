//! `loopherd pause` and `resume`: a paused herd's `feed` spawns no new workers (D23).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::herd::{self, Paths};
use crate::ledger::{FileLedger, Ledger};

/// What a herd's pause file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    /// RFC 3339, UTC.
    pub since: String,
    pub reason: Option<String>,
}

impl fmt::Display for Pause {
    /// `paused since <time>`, then ` — <reason>` when there is one.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "paused since {}", self.since)?;
        match &self.reason {
            Some(reason) => write!(f, " — {reason}"),
            None => Ok(()),
        }
    }
}

/// Which herds a pause or resume acts on.
pub enum Herds<'a> {
    One(&'a str),
    /// Every herd with a herd file now.
    All,
}

/// `$XDG_STATE_HOME/loopherd/paused`, beside the ledger.
pub fn dir() -> Result<PathBuf> {
    Ok(FileLedger::default_path()?.with_file_name("paused"))
}

/// `herd`'s pause, or `None` when it isn't paused.
pub fn paused(dir: &Path, herd: &str) -> Result<Option<Pause>> {
    let file = dir.join(herd);
    let text = match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        text => text.with_context(|| format!("read {}", file.display()))?,
    };
    let pause = serde_json::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
    Ok(Some(pause))
}

/// Pauses each herd, keeping an existing pause's time and replacing its
/// reason only when `reason` is given. One line per herd.
pub fn pause(
    paths: &Paths,
    herds: Herds,
    reason: Option<&str>,
    now: SystemTime,
) -> Result<Vec<String>> {
    let mut lines = vec![];
    for herd in resolve(paths, herds)? {
        let line = match paused(&paths.paused, &herd)? {
            Some(old) => {
                let pause = Pause {
                    reason: reason.map(String::from).or(old.reason),
                    ..old
                };
                write(&paths.paused, &herd, &pause)?;
                format!("{herd} already {pause}")
            }
            None => {
                let pause = Pause {
                    since: humantime::format_rfc3339_seconds(now).to_string(),
                    reason: reason.map(String::from),
                };
                write(&paths.paused, &herd, &pause)?;
                format!("{herd} now {pause}")
            }
        };
        lines.push(line);
    }
    Ok(lines)
}

/// Resumes each herd, with the next step that gets it workers again. One
/// line per herd; prompts nobody.
pub fn resume(paths: &Paths, ledger: &dyn Ledger, herds: Herds) -> Result<Vec<String>> {
    let entries = ledger.entries().context("ledger")?;
    let mut lines = vec![];
    for herd in resolve(paths, herds)? {
        let file = paths.paused.join(&herd);
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                lines.push(format!("{herd} is not paused"));
                continue;
            }
            removed => removed.with_context(|| format!("remove {}", file.display()))?,
        }
        let lead = entries
            .values()
            .find(|e| e.is_lead() && !e.retired && e.herd.as_deref() == Some(&herd));
        lines.push(match lead {
            Some(lead) => format!(
                "resumed {herd}; {}'s watch sees claimable tasks within 60s",
                lead.name
            ),
            None => format!("resumed {herd}; run `loopherd feed {herd}` to spawn workers"),
        });
    }
    Ok(lines)
}

/// The herds named, each checked to have a herd file.
fn resolve(paths: &Paths, herds: Herds) -> Result<Vec<String>> {
    match herds {
        Herds::One(herd) => {
            herd::require(paths, herd)?;
            Ok(vec![herd.into()])
        }
        Herds::All => herd::names(paths),
    }
}

/// Writes to a temp file and renames it, so a reader never sees half a pause.
fn write(dir: &Path, herd: &str, pause: &Pause) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join(herd);
    let temp = dir.join(format!(".{herd}.tmp"));
    std::fs::write(&temp, serde_json::to_string(pause)? + "\n")
        .with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &file).with_context(|| format!("write {}", file.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};

    struct Setup {
        dir: tempfile::TempDir,
    }

    impl Setup {
        /// Herd files for each of `herds`.
        fn new(herds: &[&str]) -> Setup {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("herds")).unwrap();
            for herd in herds {
                let file = dir.path().join(format!("herds/{herd}.toml"));
                fs::write(file, "repo = \"/r\"\n").unwrap();
            }
            Setup { dir }
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.dir.path().join("herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: self.dir.path().join("feed.lock"),
                paused: self.dir.path().join("paused"),
            }
        }

        fn paused(&self, herd: &str) -> Option<Pause> {
            paused(&self.paths().paused, herd).unwrap()
        }
    }

    fn at(rfc3339: &str) -> SystemTime {
        humantime::parse_rfc3339(rfc3339).unwrap()
    }

    fn member(name: &str, herd: &str, lead: bool, retired: bool) -> AgentRecord {
        serde_json::from_value(serde_json::json!({
            "name": name, "pane_id": "w1:p1", "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd, "retired": retired,
            "role": if lead { Some("lead") } else { None },
        }))
        .unwrap()
    }

    fn ledger(members: &[AgentRecord]) -> MemLedger {
        let entries = members.iter().map(|m| (m.name.clone(), m.clone()));
        MemLedger(entries.collect(), vec![])
    }

    #[test]
    fn pause_writes_the_herds_pause_file_with_the_time_and_reason() {
        let s = Setup::new(&["prs"]);
        let lines = pause(
            &s.paths(),
            Herds::One("prs"),
            Some("release freeze"),
            at("2026-11-05T09:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            lines,
            ["prs now paused since 2026-11-05T09:00:00Z — release freeze"]
        );
        let file = fs::read_to_string(s.dir.path().join("paused/prs")).unwrap();
        assert!(file.contains("2026-11-05T09:00:00Z"), "{file}");
        assert!(file.contains("release freeze"), "{file}");
        assert_eq!(
            s.paused("prs"),
            Some(Pause {
                since: "2026-11-05T09:00:00Z".into(),
                reason: Some("release freeze".into()),
            })
        );
    }

    #[test]
    fn an_unpaused_herd_has_no_pause() {
        let s = Setup::new(&["prs"]);
        assert_eq!(s.paused("prs"), None);
    }

    #[test]
    fn pausing_a_paused_herd_keeps_its_time_and_replaces_the_reason_only_when_given() {
        let s = Setup::new(&["prs"]);
        let first = at("2026-11-05T09:00:00Z");
        let later = first + Duration::from_secs(3600);
        pause(&s.paths(), Herds::One("prs"), Some("freeze"), first).unwrap();

        let kept = pause(&s.paths(), Herds::One("prs"), None, later).unwrap();
        assert_eq!(
            kept,
            ["prs already paused since 2026-11-05T09:00:00Z — freeze"]
        );

        let replaced = pause(&s.paths(), Herds::One("prs"), Some("outage"), later).unwrap();
        assert_eq!(
            replaced,
            ["prs already paused since 2026-11-05T09:00:00Z — outage"]
        );
        assert_eq!(
            s.paused("prs"),
            Some(Pause {
                since: "2026-11-05T09:00:00Z".into(),
                reason: Some("outage".into()),
            })
        );
    }

    #[test]
    fn a_herd_with_no_herd_file_is_refused_and_nothing_is_written() {
        let s = Setup::new(&["prs"]);
        let now = at("2026-11-05T09:00:00Z");
        let err = pause(&s.paths(), Herds::One("nope"), None, now).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.starts_with("herd: read "), "{err}");
        assert!(err.contains("nope.toml"), "{err}");
        assert!(!s.dir.path().join("paused").exists());

        let err = resume(&s.paths(), &ledger(&[]), Herds::One("nope")).unwrap_err();
        assert!(format!("{err:#}").starts_with("herd: read "), "{err:#}");
    }

    #[test]
    fn all_pauses_and_resumes_one_file_per_herd_file_that_exists_now() {
        let s = Setup::new(&["docs", "prs"]);
        let now = at("2026-11-05T09:00:00Z");
        let lines = pause(&s.paths(), Herds::All, None, now).unwrap();
        assert_eq!(
            lines,
            [
                "docs now paused since 2026-11-05T09:00:00Z",
                "prs now paused since 2026-11-05T09:00:00Z"
            ]
        );
        assert!(s.paused("docs").is_some() && s.paused("prs").is_some());

        fs::write(s.dir.path().join("herds/later.toml"), "repo = \"/r\"\n").unwrap();
        assert_eq!(s.paused("later"), None);

        resume(&s.paths(), &ledger(&[]), Herds::All).unwrap();
        assert_eq!((s.paused("docs"), s.paused("prs")), (None, None));
    }

    #[test]
    fn resume_deletes_the_pause_file_and_points_an_unled_herd_at_feed() {
        let s = Setup::new(&["prs"]);
        pause(&s.paths(), Herds::One("prs"), None, SystemTime::now()).unwrap();
        let retired_lead = member("lead-prs", "prs", true, true);
        let lines = resume(&s.paths(), &ledger(&[retired_lead]), Herds::One("prs")).unwrap();
        assert_eq!(
            lines,
            ["resumed prs; run `loopherd feed prs` to spawn workers"]
        );
        assert_eq!(s.paused("prs"), None);
        assert!(!s.dir.path().join("paused/prs").exists());
    }

    #[test]
    fn resume_tells_a_led_herd_its_leads_watch_sees_claimable_tasks_within_60s() {
        let s = Setup::new(&["prs"]);
        pause(&s.paths(), Herds::One("prs"), None, SystemTime::now()).unwrap();
        let members = [
            member("lead-docs", "docs", true, false),
            member("lead-prs", "prs", true, false),
            member("pr-1", "prs", false, false),
        ];
        let lines = resume(&s.paths(), &ledger(&members), Herds::One("prs")).unwrap();
        assert_eq!(
            lines,
            ["resumed prs; lead-prs's watch sees claimable tasks within 60s"]
        );
    }

    #[test]
    fn resuming_an_unpaused_herd_says_so() {
        let s = Setup::new(&["prs"]);
        let lines = resume(&s.paths(), &ledger(&[]), Herds::One("prs")).unwrap();
        assert_eq!(lines, ["prs is not paused"]);
    }
}
