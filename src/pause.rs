//! `loopherd pause` and `resume`: a paused herd's `feed` spawns no new workers (D23).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::herd::{self, Paths};
use crate::ledger::{FileLedger, Ledger};

/// A herd's pause as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pause {
    /// RFC 3339, UTC; `None` when the pause file couldn't be read.
    pub since: Option<String>,
    pub reason: Option<String>,
}

/// What a pause file holds.
#[derive(Serialize, Deserialize)]
struct Record {
    since: String,
    reason: Option<String>,
}

impl Pause {
    /// `since <time>`, or `(time unknown)`.
    pub fn when(&self) -> String {
        match &self.since {
            Some(since) => format!("since {}", escaped(since)),
            None => "(time unknown)".into(),
        }
    }
}

impl fmt::Display for Pause {
    /// `paused since <time>`, then ` — <reason>` when there is one.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "paused {}", self.when())?;
        match &self.reason {
            Some(reason) => write!(f, " — {}", escaped(reason)),
            None => Ok(()),
        }
    }
}

/// A herd's pause file, as read.
#[derive(Debug)]
pub enum Reading {
    Unpaused,
    Paused(Pause),
    Unreadable(anyhow::Error),
}

impl Reading {
    /// The herd's pause. An unreadable pause file fails closed: paused, its time unknown.
    pub fn pause(&self) -> Option<Pause> {
        match self {
            Reading::Unpaused => None,
            Reading::Paused(pause) => Some(pause.clone()),
            Reading::Unreadable(_) => Some(Pause {
                since: None,
                reason: None,
            }),
        }
    }

    /// For an unreadable pause file, a warning naming `herd`.
    pub fn warning(&self, herd: &str) -> Option<String> {
        match self {
            Reading::Unreadable(e) => Some(format!(
                "warning: {herd}'s pause is unreadable, counted as paused: {e:#}"
            )),
            _ => None,
        }
    }
}

/// Which herds a pause or resume acts on.
pub enum Herds<'a> {
    One(&'a str),
    /// Every herd with a herd file now.
    All,
}

/// One line per herd, and 1 if any herd failed.
#[derive(Debug)]
pub struct Done {
    pub lines: Vec<String>,
    pub code: i32,
}

/// Where `status` and `view` find pauses, and the herd files that make a
/// pause worth showing.
pub struct Pauses {
    pub dir: PathBuf,
    /// `None` when the environment names no config dir.
    pub herds: Option<PathBuf>,
}

impl Pauses {
    /// Needs `$HOME` only when neither `$XDG_STATE_HOME` nor a config dir is set.
    pub fn from_env() -> Result<Pauses> {
        Ok(Pauses {
            dir: dir()?,
            herds: herd::herds_dir(),
        })
    }

    /// Each herd's pause, for the herds in `known` and those with a herd file.
    /// When the pause dir can't be listed, each of those herds' files is read
    /// by name, so a pause fails closed as it does for `feed`.
    pub fn of(&self, known: impl IntoIterator<Item = String>) -> Vec<(String, Reading)> {
        let mut herds: BTreeSet<String> = known.into_iter().collect();
        if let Some(dir) = &self.herds {
            herds.extend(herd::names(dir).unwrap_or_default());
        }
        match all(&self.dir) {
            Ok(pauses) => pauses
                .into_iter()
                .filter(|(h, _)| herds.contains(h))
                .collect(),
            Err(_) => herds
                .into_iter()
                .map(|h| (h.clone(), read(&self.dir, &h)))
                .collect(),
        }
    }
}

#[cfg(test)]
impl Pauses {
    /// Pauses in `root/paused`, herd files in `root/herds`.
    pub fn under(root: &Path) -> Pauses {
        Pauses {
            dir: root.join("paused"),
            herds: Some(root.join("herds")),
        }
    }
}

/// `$XDG_STATE_HOME/loopherd/paused`, beside the ledger.
pub fn dir() -> Result<PathBuf> {
    Ok(FileLedger::default_path()?.with_file_name("paused"))
}

/// `herd`'s pause file; a name that can't be a herd's is never paused.
pub fn read(dir: &Path, herd: &str) -> Reading {
    if !herd::is_name(herd) {
        return Reading::Unpaused;
    }
    let file = dir.join(herd);
    let text = match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Reading::Unpaused,
        text => text.with_context(|| format!("read {}", file.display())),
    };
    let record = text.and_then(|text| {
        let record: Record = serde_json::from_str(&text)?;
        humantime::parse_rfc3339(&record.since)
            .with_context(|| format!("since {:?} is not RFC 3339", record.since))?;
        Ok(record)
    });
    match record.with_context(|| format!("parse {}", file.display())) {
        Ok(Record { since, reason }) => Reading::Paused(Pause {
            since: Some(since),
            reason,
        }),
        Err(e) => Reading::Unreadable(e),
    }
}

/// Every pause file by herd name.
pub fn all(dir: &Path) -> Result<Vec<(String, Reading)>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        entries => entries.with_context(|| format!("read {}", dir.display()))?,
    };
    let mut names = vec![];
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if herd::is_name(&name) {
            names.push(name);
        }
    }
    names.sort();
    let read = |herd: String| {
        let reading = read(dir, &herd);
        (herd, reading)
    };
    Ok(names.into_iter().map(read).collect())
}

/// Pauses each herd, keeping an existing pause's time and replacing its
/// reason only when `reason` is given; an empty one clears it. An unreadable
/// pause file is replaced by a fresh pause. A herd that fails doesn't stop the rest.
pub fn pause(paths: &Paths, herds: Herds, reason: Option<&str>, now: SystemTime) -> Result<Done> {
    if reason.is_some_and(|r| r.chars().any(unprintable)) {
        bail!("a reason can't hold control or format characters such as a newline");
    }
    let given = reason.map(|r| Some(r).filter(|r| !r.is_empty()).map(String::from));
    let mut lines = vec![];
    let herds = resolve(paths, herds, &mut lines)?;
    Ok(each(lines, herds, |herd| {
        let (record, verb) = match read(&paths.paused, herd) {
            Reading::Paused(Pause {
                since: Some(since),
                reason,
            }) => {
                let reason = given.clone().unwrap_or(reason);
                (Record { since, reason }, "already")
            }
            _ => {
                let since = humantime::format_rfc3339_seconds(now).to_string();
                let reason = given.clone().flatten();
                (Record { since, reason }, "now")
            }
        };
        write(&paths.paused, herd, &record)?;
        let pause = Pause {
            since: Some(record.since),
            reason: record.reason,
        };
        Ok(format!("{herd} {verb} {pause}"))
    }))
}

/// Resumes each herd, with the next step that gets it workers again; prompts
/// nobody. A named herd's pause file goes even when its herd file is gone.
/// A herd that fails doesn't stop the rest.
pub fn resume(paths: &Paths, ledger: &dyn Ledger, herds: Herds) -> Result<Done> {
    let entries = ledger.entries().context("ledger")?;
    let mut lines = vec![];
    let herds = match herds {
        Herds::One(herd) if herd::is_name(herd) && paths.paused.join(herd).exists() => {
            vec![herd.to_string()]
        }
        herds => resolve(paths, herds, &mut lines)?,
    };
    Ok(each(lines, herds, |herd| {
        let file = paths.paused.join(herd);
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(format!("{herd} is not paused"));
            }
            removed => removed.with_context(|| format!("remove {}", file.display()))?,
        }
        let lead = entries
            .values()
            .find(|e| e.is_lead() && !e.retired && e.herd.as_deref() == Some(herd));
        Ok(match lead {
            Some(lead) => format!(
                "resumed {herd}; {}'s watch sees claimable tasks within 60s",
                lead.name
            ),
            None if herd::require(paths, herd).is_err() => {
                format!("resumed {herd}; it has no herd file, so nothing feeds it")
            }
            None => format!("resumed {herd}; run `loopherd feed {herd}` to spawn workers"),
        })
    }))
}

/// Runs `act` on every herd, a line each, an error's line naming its herd.
fn each(
    mut lines: Vec<String>,
    herds: Vec<String>,
    mut act: impl FnMut(&str) -> Result<String>,
) -> Done {
    let mut code = 0;
    for herd in herds {
        lines.push(act(&herd).unwrap_or_else(|e| {
            code = 1;
            format!("error: {herd}: {e:#}")
        }));
    }
    Done { lines, code }
}

/// The herds named, each checked to have a herd file; with none for `--all`,
/// a hint in `lines`.
fn resolve(paths: &Paths, herds: Herds, lines: &mut Vec<String>) -> Result<Vec<String>> {
    match herds {
        Herds::One(herd) => {
            herd::require(paths, herd)?;
            Ok(vec![herd.into()])
        }
        Herds::All => {
            let names = herd::names(&paths.herds)?;
            if names.is_empty() {
                lines.push(format!(
                    "no herds: add a herd file at {}/<herd>.toml",
                    paths.herds.display()
                ));
            }
            Ok(names)
        }
    }
}

/// Writes to a temp file and renames it, so a reader never sees half a pause.
fn write(dir: &Path, herd: &str, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join(herd);
    let temp = dir.join(format!(".{herd}.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_string(record)? + "\n")
        .with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &file)
        .inspect_err(|_| _ = std::fs::remove_file(&temp))
        .with_context(|| format!("write {}", file.display()))
}

/// `text` with each control, format or line/paragraph separator as `\u{..}`,
/// so a hand-edited pause file can't move the terminal or split a line.
fn escaped(text: &str) -> String {
    text.chars()
        .map(|c| match unprintable(c) {
            true => c.escape_unicode().to_string(),
            false => c.to_string(),
        })
        .collect()
}

/// Unicode's Cc, Cf, Zl and Zp characters.
fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{AD}'
                | '\u{600}'..='\u{605}'
                | '\u{61C}'
                | '\u{6DD}'
                | '\u{70F}'
                | '\u{890}'..='\u{891}'
                | '\u{8E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
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
            Paths::under(self.dir.path())
        }

        fn paused(&self, herd: &str) -> Option<Pause> {
            read(&self.paths().paused, herd).pause()
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
        .unwrap()
        .lines;
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
                since: Some("2026-11-05T09:00:00Z".into()),
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

        let kept = pause(&s.paths(), Herds::One("prs"), None, later)
            .unwrap()
            .lines;
        assert_eq!(
            kept,
            ["prs already paused since 2026-11-05T09:00:00Z — freeze"]
        );

        let replaced = pause(&s.paths(), Herds::One("prs"), Some("outage"), later)
            .unwrap()
            .lines;
        assert_eq!(
            replaced,
            ["prs already paused since 2026-11-05T09:00:00Z — outage"]
        );
        assert_eq!(
            s.paused("prs"),
            Some(Pause {
                since: Some("2026-11-05T09:00:00Z".into()),
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
        let lines = pause(&s.paths(), Herds::All, None, now).unwrap().lines;
        assert_eq!(
            lines,
            [
                "docs now paused since 2026-11-05T09:00:00Z",
                "prs now paused since 2026-11-05T09:00:00Z"
            ]
        );
        assert!(s.paused("docs").is_some() && s.paused("prs").is_some());

        fs::write(s.dir.path().join("herds/later.toml"), "repo = \"/r\"\n").unwrap();
        let files = fs::read_dir(s.dir.path().join("paused")).unwrap();
        let mut files: Vec<_> = files.map(|f| f.unwrap().file_name()).collect();
        files.sort();
        assert_eq!(files, ["docs", "prs"]);
        assert_eq!(s.paused("later"), None);

        resume(&s.paths(), &ledger(&[]), Herds::All).unwrap();
        assert_eq!((s.paused("docs"), s.paused("prs")), (None, None));
    }

    #[test]
    fn resume_deletes_the_pause_file_and_points_an_unled_herd_at_feed() {
        let s = Setup::new(&["prs"]);
        pause(&s.paths(), Herds::One("prs"), None, SystemTime::now()).unwrap();
        let retired_lead = member("lead-prs", "prs", true, true);
        let lines = resume(&s.paths(), &ledger(&[retired_lead]), Herds::One("prs"))
            .unwrap()
            .lines;
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
        let lines = resume(&s.paths(), &ledger(&members), Herds::One("prs"))
            .unwrap()
            .lines;
        assert_eq!(
            lines,
            ["resumed prs; lead-prs's watch sees claimable tasks within 60s"]
        );
    }

    #[test]
    fn an_empty_reason_clears_the_reason() {
        let s = Setup::new(&["prs"]);
        let now = at("2026-11-05T09:00:00Z");
        pause(&s.paths(), Herds::One("prs"), Some("freeze"), now).unwrap();
        let lines = pause(&s.paths(), Herds::One("prs"), Some(""), now)
            .unwrap()
            .lines;
        assert_eq!(lines, ["prs already paused since 2026-11-05T09:00:00Z"]);
        assert_eq!(s.paused("prs").unwrap().reason, None);

        let s = Setup::new(&["prs"]);
        pause(&s.paths(), Herds::One("prs"), Some(""), now).unwrap();
        assert_eq!(s.paused("prs").unwrap().reason, None);
    }

    #[test]
    fn all_with_no_herd_files_says_so() {
        let s = Setup::new(&[]);
        let now = SystemTime::now();
        let hint = format!(
            "no herds: add a herd file at {}/<herd>.toml",
            s.paths().herds.display()
        );
        assert_eq!(
            pause(&s.paths(), Herds::All, None, now).unwrap().lines,
            std::slice::from_ref(&hint)
        );
        assert_eq!(
            resume(&s.paths(), &ledger(&[]), Herds::All).unwrap().lines,
            [hint]
        );
    }

    #[test]
    fn all_lists_every_pause_by_name_with_unreadable_ones_marked() {
        let s = Setup::new(&["docs", "prs"]);
        let now = at("2026-11-05T09:00:00Z");
        pause(&s.paths(), Herds::One("prs"), None, now).unwrap();
        fs::write(s.dir.path().join("paused/docs"), "").unwrap();
        fs::write(s.dir.path().join("paused/.prs.1.tmp"), "").unwrap();

        let listed = all(&s.paths().paused).unwrap();

        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["docs", "prs"]);
        assert!(matches!(listed[0].1, Reading::Unreadable(_)));
        let since = listed[1].1.pause().unwrap().since;
        assert_eq!(since.as_deref(), Some("2026-11-05T09:00:00Z"));
    }

    #[test]
    fn a_name_that_is_not_a_herd_name_is_never_paused() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();
        assert_eq!(s.paused(".."), None);
        assert_eq!(s.paused(""), None);
    }

    #[test]
    fn resuming_an_unpaused_herd_says_so() {
        let s = Setup::new(&["prs"]);
        let lines = resume(&s.paths(), &ledger(&[]), Herds::One("prs"))
            .unwrap()
            .lines;
        assert_eq!(lines, ["prs is not paused"]);
    }

    #[test]
    fn an_unreadable_pause_counts_as_paused_with_its_time_unknown() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();
        fs::write(s.dir.path().join("paused/prs"), "").unwrap();

        let reading = read(&s.paths().paused, "prs");

        let pause = reading.pause().unwrap();
        assert_eq!(pause.to_string(), "paused (time unknown)");
        let warning = reading.warning("prs").unwrap();
        assert!(
            warning.starts_with("warning: prs's pause is unreadable, counted as paused: parse "),
            "{warning}"
        );
    }

    #[test]
    fn pausing_over_an_unreadable_pause_writes_a_fresh_one() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();
        fs::write(s.dir.path().join("paused/prs"), "").unwrap();
        let now = at("2026-11-05T09:00:00Z");

        let done = pause(&s.paths(), Herds::One("prs"), Some("freeze"), now).unwrap();

        assert_eq!(
            done.lines,
            ["prs now paused since 2026-11-05T09:00:00Z — freeze"]
        );
        assert_eq!(done.code, 0);
        let since = s.paused("prs").unwrap().since;
        assert_eq!(since.as_deref(), Some("2026-11-05T09:00:00Z"));
    }

    #[test]
    fn all_reports_every_herd_and_exits_1_when_one_fails() {
        let s = Setup::new(&["docs", "mid", "prs"]);
        let blocker = s.dir.path().join("paused/mid/blocker");
        fs::create_dir_all(&blocker).unwrap();
        let now = at("2026-11-05T09:00:00Z");

        let paused = pause(&s.paths(), Herds::All, None, now).unwrap();

        assert_eq!(paused.code, 1);
        assert_eq!(paused.lines.len(), 3, "{:?}", paused.lines);
        assert_eq!(
            paused.lines[0],
            "docs now paused since 2026-11-05T09:00:00Z"
        );
        assert!(
            paused.lines[1].starts_with("error: mid: write "),
            "{:?}",
            paused.lines
        );
        assert_eq!(paused.lines[2], "prs now paused since 2026-11-05T09:00:00Z");

        let resumed = resume(&s.paths(), &ledger(&[]), Herds::All).unwrap();

        assert_eq!(resumed.code, 1);
        assert_eq!(resumed.lines.len(), 3, "{:?}", resumed.lines);
        assert_eq!(
            resumed.lines[0],
            "resumed docs; run `loopherd feed docs` to spawn workers"
        );
        assert!(
            resumed.lines[1].starts_with("error: mid: remove "),
            "{:?}",
            resumed.lines
        );
        assert_eq!(
            resumed.lines[2],
            "resumed prs; run `loopherd feed prs` to spawn workers"
        );
    }

    #[test]
    fn resume_deletes_the_pause_of_a_herd_whose_herd_file_is_gone() {
        let s = Setup::new(&["prs"]);
        pause(&s.paths(), Herds::One("prs"), None, SystemTime::now()).unwrap();
        fs::remove_file(s.dir.path().join("herds/prs.toml")).unwrap();

        let done = resume(&s.paths(), &ledger(&[]), Herds::One("prs")).unwrap();

        assert_eq!(
            done.lines,
            ["resumed prs; it has no herd file, so nothing feeds it"]
        );
        assert!(!s.dir.path().join("paused/prs").exists());
    }

    #[test]
    fn resume_validates_the_name_before_looking_for_a_pause_file() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();

        let err = resume(&s.paths(), &ledger(&[]), Herds::One("..")).unwrap_err();

        assert!(format!("{err:#}").contains("is not a herd name"), "{err:#}");
        assert!(s.dir.path().join("paused").exists());
    }

    #[test]
    fn a_reason_with_a_control_character_is_refused_and_nothing_is_written() {
        let s = Setup::new(&["prs"]);
        for reason in [
            "freeze\nprs",
            "freeze\r",
            "a\tb",
            "a\u{2028}b",
            "a\u{2029}b",
            "a\u{202E}b",
            "a\u{200B}b",
        ] {
            let now = SystemTime::now();
            let err = pause(&s.paths(), Herds::One("prs"), Some(reason), now).unwrap_err();
            assert_eq!(
                err.to_string(),
                "a reason can't hold control or format characters such as a newline"
            );
        }
        assert!(!s.dir.path().join("paused").exists());
    }

    #[test]
    fn a_pause_file_without_an_rfc_3339_since_is_unreadable() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();
        for text in [
            "{}",
            r#"{"since": null}"#,
            r#"{"since": "yesterday", "reason": null}"#,
        ] {
            fs::write(s.dir.path().join("paused/prs"), text).unwrap();
            let reading = read(&s.paths().paused, "prs");
            assert!(
                matches!(reading, Reading::Unreadable(_)),
                "{text}: {reading:?}"
            );
            assert_eq!(reading.pause().unwrap().since, None, "{text}");
        }
    }

    #[test]
    fn pausing_over_a_pause_with_no_since_repairs_it() {
        let s = Setup::new(&["prs"]);
        fs::create_dir(s.dir.path().join("paused")).unwrap();
        fs::write(s.dir.path().join("paused/prs"), r#"{"since": null}"#).unwrap();
        let now = at("2026-11-05T09:00:00Z");

        let done = pause(&s.paths(), Herds::One("prs"), None, now).unwrap();

        assert_eq!(done.lines, ["prs now paused since 2026-11-05T09:00:00Z"]);
        let file = fs::read_to_string(s.dir.path().join("paused/prs")).unwrap();
        assert!(file.contains(r#""since":"2026-11-05T09:00:00Z""#), "{file}");
    }

    #[test]
    fn a_hand_edited_reason_prints_its_control_and_format_characters_escaped() {
        let pause = Pause {
            since: Some("2026-11-05T09:00:00Z".into()),
            reason: Some("a\u{1b}[31mb\nc\u{202E}d".into()),
        };
        assert_eq!(
            pause.to_string(),
            r"paused since 2026-11-05T09:00:00Z — a\u{1b}[31mb\u{a}c\u{202e}d"
        );
    }
}
