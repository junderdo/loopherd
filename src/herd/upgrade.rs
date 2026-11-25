//! `loopherd herd upgrade`: offer a herd the named changes newer init
//! writes that it lacks, one at a time (D157).

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Value, value};

use super::edit::{edit, replace};
use super::{Paths, SwitchValues, expand_home, file_of, lead_defaults, load, names};
use prompt::{Anchored, Place, Placed};

mod prompt;

/// How an upgrade runs.
pub struct Upgrade<'a> {
    /// Herds whose lead is in the ledger and not retired.
    pub running_leads: &'a BTreeSet<String>,
    /// Stamps the `.bak-` copy taken before a file's first write.
    pub now: SystemTime,
    /// Where answers are read; without one nothing is applied and each herd's
    /// pending ids are listed.
    pub terminal: Option<&'a mut dyn BufRead>,
}

/// One change newer init writes. Whether a herd has it is read from the
/// file itself, so a hand edit counts as applied.
struct Migration {
    id: &'static str,
    /// What it does and why, in at most two lines.
    explanation: &'static str,
    /// Whether a running lead only sees the change once restarted or revived.
    lead_reads_at_start: bool,
    state: fn(&Herd) -> State,
}

/// What a migration reads to decide whether a herd has it.
struct Herd<'a> {
    doc: &'a DocumentMut,
    defaults: &'a SwitchValues,
    paths: &'a Paths,
}

enum State {
    /// Already there, or not for this herd.
    Current,
    Pending(Change),
    /// The change can't be placed without guessing, so it's shown, not made.
    Tailored {
        reason: String,
        place: String,
        snippet: String,
    },
}

enum Change {
    Herd(Box<dyn Fn(&mut DocumentMut)>),
    /// Worked out again on the file as it reads at each use.
    Prompt {
        path: PathBuf,
        anchored: &'static Anchored,
    },
}

const MIGRATIONS: [Migration; 4] = [
    Migration {
        id: "lead-review-rounds",
        explanation: "Writes the lead's review-round limit into its prompt where init puts it,\n\
                      so the lead reads its {review_rounds} beside its switches.",
        lead_reads_at_start: true,
        state: |herd| lead_prompt(herd, &REVIEW_ROUNDS),
    },
    Migration {
        id: "lead-drained",
        explanation: "Tells the lead to report done and stop on `<herd> drained`,\n\
                      instead of re-arming its watch after every exit.",
        lead_reads_at_start: true,
        state: |herd| lead_prompt(herd, &DRAINED),
    },
    Migration {
        id: "lead-follow-ups-ready",
        explanation: "Writes the lead's follow_ups_ready switch as [lead_defaults] sets it, else off.\n\
                      On, the lead files a reviewer's in-scope follow-ups ready for agents.",
        lead_reads_at_start: true,
        state: follow_ups_ready,
    },
    Migration {
        id: "merged-repo",
        explanation: "Names the repo in [template.issue]'s merged check with --repo <owner>/<name>,\n\
                      so it works from any directory and behind an ssh host alias.",
        lead_reads_at_start: false,
        state: merged_repo,
    },
];

const REVIEW_ROUNDS: Anchored = Anchored {
    has: "{review_rounds}",
    anchor: "Grilling is always the human's.",
    place: Place::Before,
    text: "Your review-round limit: {review_rounds}.\n\n",
};

const DRAINED: Anchored = Anchored {
    has: "{herd} drained",
    anchor: "re-arm after every exit.",
    place: Place::Replace,
    text: "re-arm after every exit but `{herd} drained`: on that, report done and\n   stop.",
};

/// `anchored` placed in the lead's prompt, inline or in its prompt file.
fn lead_prompt(herd: &Herd, anchored: &'static Anchored) -> State {
    let Some(lead) = herd.doc.get("lead").and_then(Item::as_table_like) else {
        return State::Current;
    };
    // How often the anchor appears; none when the quoting can't hold the change.
    let (matches, source) = match (lead.get("prompt"), lead.get("prompt_file")) {
        (Some(inline), _) => match inline.as_value().and_then(|v| anchored.place_in_toml(v)) {
            None | Some(Placed::Has) => return State::Current,
            Some(Placed::Edited(_)) => {
                return State::Pending(Change::Herd(Box::new(|doc| place_inline(doc, anchored))));
            }
            Some(Placed::Unplaced { matches }) => (Some(matches), "[lead]'s prompt".to_string()),
            Some(Placed::Unquotable) => (None, "[lead]'s prompt".to_string()),
        },
        (None, Some(file)) => {
            let Some(file) = file.as_str() else {
                return State::Current;
            };
            let path = herd.paths.herds.join(expand_home(file, &herd.paths.home));
            // An unreadable prompt fails the herd's checks, which say so.
            let Ok(text) = fs::read_to_string(&path) else {
                return State::Current;
            };
            match anchored.place(&text) {
                Placed::Has => return State::Current,
                Placed::Edited(_) => return State::Pending(Change::Prompt { path, anchored }),
                Placed::Unplaced { matches } => (Some(matches), path.display().to_string()),
                Placed::Unquotable => (None, path.display().to_string()),
            }
        }
        (None, None) => return State::Current,
    };
    let anchor = anchored.anchor;
    let reason = match matches {
        Some(0) => format!("{anchor:?} is not in {source}"),
        Some(n) => format!("{anchor:?} appears {n} times in {source}"),
        None => format!("{source}'s quoting can't hold it as written"),
    };
    State::Tailored {
        reason,
        place: format!("{source}, {}", anchored.place_name()),
        snippet: anchored.text.to_string(),
    }
}

/// Places `anchored` in the inline prompt as it reads now, so a hand edit
/// made while the offer waited is kept.
fn place_inline(doc: &mut DocumentMut, anchored: &Anchored) {
    let Some(lead) = doc.get_mut("lead").and_then(Item::as_table_like_mut) else {
        return;
    };
    let placed = lead
        .get("prompt")
        .and_then(Item::as_value)
        .and_then(|v| anchored.place_in_toml(v));
    if let Some(Placed::Edited(new)) = placed {
        lead.insert("prompt", Item::Value(new));
    }
}

/// `path` as it reads now and with `anchored` placed; none when it can't be.
fn placed_in_file(path: &Path, anchored: &Anchored) -> Result<Option<(String, String)>> {
    let was = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(match anchored.place(&was) {
        Placed::Edited(now) => Some((was, now)),
        _ => None,
    })
}

fn follow_ups_ready(herd: &Herd) -> State {
    // As init writes it, so a lead inheriting the switch keeps it.
    let on = herd.defaults.follow_ups_ready.unwrap_or(false);
    match herd.doc.get("lead").and_then(Item::as_table_like) {
        Some(lead) if !lead.contains_key("follow_ups_ready") => {
            State::Pending(Change::Herd(Box::new(move |doc| {
                if let Some(lead) = doc.get_mut("lead").and_then(Item::as_table_like_mut) {
                    lead.insert("follow_ups_ready", value(on));
                }
            })))
        }
        _ => State::Current,
    }
}

fn merged_repo(herd: &Herd) -> State {
    let merged = herd
        .doc
        .get("template")
        .and_then(|t| t.get("issue"))
        .and_then(|t| t.get("merged"))
        .and_then(Item::as_str);
    let Some(merged) = merged else {
        return State::Current;
    };
    let names_repo = merged
        .split_whitespace()
        .any(|word| word == "--repo" || word == "-R" || word.starts_with("--repo="));
    if names_repo || !merged.contains("gh pr list") {
        return State::Current;
    }
    let token = merged
        .split_once("gh pr list")
        .map(|(before, _)| before)
        .filter(|before| before.starts_with("GH_TOKEN="))
        .unwrap_or_default();
    let command = format!(
        "{token}gh pr list --repo <owner>/<name> --head {{branch}} --state merged \
         --json number --jq 'length > 0' | grep -qx true"
    );
    State::Tailored {
        reason: "this herd's own value is kept".into(),
        place: "[template.issue]".into(),
        snippet: format!("merged = {}", Value::from(command)),
    }
}

/// Offers `herd`, or every herd, its pending migrations, writing to `out`.
pub fn upgrade(
    paths: &Paths,
    herd: Option<&str>,
    mut how: Upgrade,
    out: &mut dyn Write,
) -> Result<()> {
    let herds = match herd {
        Some(herd) => vec![herd.to_string()],
        None => names(&paths.herds)?,
    };
    let mut backups = Backups::new(how.now);
    for herd in &herds {
        if let Err(invalid) = super::checked(&file_of(&paths.herds, herd)?, paths) {
            writeln!(out, "{herd}: skipped, {invalid}")?;
            continue;
        }
        upgrade_herd(paths, herd, &mut how, &mut backups, out)
            .with_context(|| format!("upgrade {herd}"))?;
    }
    Ok(())
}

fn upgrade_herd(
    paths: &Paths,
    herd: &str,
    how: &mut Upgrade,
    backups: &mut Backups,
    out: &mut dyn Write,
) -> Result<()> {
    let file = file_of(&paths.herds, herd)?;
    let declined = load(&paths.herds, herd)?.declined;
    let defaults = lead_defaults(paths)?;
    let state = |migration: &Migration, doc: &DocumentMut| {
        (migration.state)(&Herd {
            doc,
            defaults: &defaults,
            paths,
        })
    };
    let doc = read(&file)?;
    let pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| !declined.iter().any(|id| id == m.id))
        .filter(|m| !matches!(state(m, &doc), State::Current))
        .collect();
    if pending.is_empty() {
        writeln!(out, "herd {herd} is up to date")?;
        return Ok(());
    }
    let Some(input) = how.terminal.as_deref_mut() else {
        let ids: Vec<_> = pending.iter().map(|m| m.id).collect();
        let features = if ids.len() == 1 {
            "feature"
        } else {
            "features"
        };
        writeln!(
            out,
            "{herd}: {} new {features}: {}",
            ids.len(),
            ids.join(", ")
        )?;
        writeln!(out, "  run: loopherd herd upgrade {herd}")?;
        return Ok(());
    };
    for migration in pending {
        // Read again: an earlier answer may have written the file.
        let doc = read(&file)?;
        let header = format!("{} — {}", migration.id, migration.explanation);
        let decline = match state(migration, &doc) {
            State::Current => continue,
            State::Pending(change) => {
                writeln!(out, "{header}")?;
                if migration.lead_reads_at_start && how.running_leads.contains(herd) {
                    writeln!(
                        out,
                        "lead-{herd} is running; it reads this at its next restart or revive."
                    )?;
                }
                let (shown, was, now) = match &change {
                    Change::Herd(change) => {
                        let mut after = doc.clone();
                        change(&mut after);
                        (&file, doc.to_string(), after.to_string())
                    }
                    Change::Prompt { path, anchored } => match placed_in_file(path, anchored)? {
                        Some((was, now)) => (path, was, now),
                        None => continue,
                    },
                };
                let shown = shown.display().to_string();
                let diff = similar::TextDiff::from_lines(&was, &now);
                write!(out, "{}", diff.unified_diff().header(&shown, &shown))?;
                if ask(input, out, "Apply?", true)? {
                    match change {
                        Change::Herd(change) => {
                            backups.take(&file)?;
                            edit(paths, herd, |doc| {
                                change(doc);
                                Ok(true)
                            })?;
                        }
                        Change::Prompt { path, anchored } => {
                            match placed_in_file(&path, anchored)? {
                                Some((_, now)) => {
                                    backups.take(&path)?;
                                    replace(&path, &now)?;
                                }
                                None => writeln!(
                                    out,
                                    "Not applied: {} changed and no longer takes it.",
                                    path.display()
                                )?,
                            }
                        }
                    }
                    false
                } else {
                    true
                }
            }
            State::Tailored {
                reason,
                place,
                snippet,
            } => {
                writeln!(out, "{header}")?;
                writeln!(out, "Not applied: {reason}. It belongs in {place}:")?;
                for line in snippet.lines() {
                    writeln!(out, "  {line}")?;
                }
                ask(input, out, "Stop offering this?", false)?
            }
        };
        if decline {
            backups.take(&file)?;
            edit(paths, herd, |doc| {
                add_declined(doc, migration.id);
                Ok(true)
            })?;
        }
    }
    Ok(())
}

fn add_declined(doc: &mut DocumentMut, id: &str) {
    match doc.get_mut("declined").and_then(Item::as_array_mut) {
        Some(declined) => declined.push(id),
        None => {
            let mut declined = Array::new();
            declined.push(id);
            doc.insert("declined", value(declined));
        }
    }
}

/// A yes or no to `question` read from `input`, Enter taking `default`.
fn ask(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    question: &str,
    default: bool,
) -> Result<bool> {
    let options = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        write!(out, "{question} {options} ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("no answer to {question:?}; stopped");
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(out, "answer y or n")?,
        }
    }
}

fn read(file: &Path) -> Result<DocumentMut> {
    let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    text.parse().with_context(|| file.display().to_string())
}

/// The copy of each file taken once per run, before its first write.
struct Backups {
    now: SystemTime,
    taken: BTreeSet<PathBuf>,
}

impl Backups {
    fn new(now: SystemTime) -> Self {
        Backups {
            now,
            taken: BTreeSet::new(),
        }
    }

    fn take(&mut self, file: &Path) -> Result<()> {
        // Writes go to a symlink's target, so that is what is kept.
        let file = fs::canonicalize(file).with_context(|| format!("read {}", file.display()))?;
        if self.taken.contains(&file) {
            return Ok(());
        }
        let stamp = humantime::format_rfc3339_seconds(self.now);
        let mut name = file.as_os_str().to_owned();
        name.push(format!(".bak-{stamp}"));
        fs::copy(&file, &name).with_context(|| format!("back up {}", file.display()))?;
        self.taken.insert(file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const LED: &str = r#"# Herd h
repo = "/src/repo"

[queue]
template = "issue"
source = "list"
max = 2

[template.issue]
params = ["number"]
prompt = "work {number}"
done = "exit 0"

[lead]
prompt = "lead {herd} to {herd} drained in {review_rounds} rounds"
# From [lead_defaults] when init ran.
merge = false # the human merges
"#;

    fn herds(text: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir(&paths.herds).unwrap();
        fs::write(paths.herds.join("h.toml"), text).unwrap();
        (dir, paths)
    }

    fn text(paths: &Paths) -> String {
        fs::read_to_string(paths.herds.join("h.toml")).unwrap()
    }

    fn at(stamp: &str) -> SystemTime {
        humantime::parse_rfc3339(stamp).unwrap()
    }

    /// Runs an upgrade of `herd` answering `answers` on a terminal; what it printed.
    fn answered(paths: &Paths, herd: Option<&str>, answers: &str, leads: &[&str]) -> String {
        let running_leads = leads.iter().map(|h| h.to_string()).collect();
        let mut input = answers.as_bytes();
        let mut out = vec![];
        let how = Upgrade {
            running_leads: &running_leads,
            now: at("2026-11-24T10:00:00Z"),
            terminal: Some(&mut input),
        };
        upgrade(paths, herd, how, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_yes_adds_follow_ups_ready_to_the_lead_keeping_comments_and_layout() {
        let (_dir, paths) = herds(LED);
        answered(&paths, Some("h"), "\n", &[]);
        assert_eq!(
            text(&paths),
            LED.replace(
                "merge = false # the human merges\n",
                "merge = false # the human merges\nfollow_ups_ready = false\n"
            )
        );
    }

    #[test]
    fn an_offer_prints_its_id_explanation_and_a_diff_of_just_that_change() {
        let (_dir, paths) = herds(LED);
        let file = paths.herds.join("h.toml").display().to_string();
        let printed = answered(&paths, Some("h"), "n\n", &[]);
        assert_eq!(
            printed,
            format!(
                "lead-follow-ups-ready — Writes the lead's follow_ups_ready switch as [lead_defaults] sets it, else off.\n\
                 On, the lead files a reviewer's in-scope follow-ups ready for agents.\n\
                 --- {file}\n\
                 +++ {file}\n\
                 @@ -15,3 +15,4 @@\n \
                 prompt = \"lead {{herd}} to {{herd}} drained in {{review_rounds}} rounds\"\n \
                 # From [lead_defaults] when init ran.\n \
                 merge = false # the human merges\n\
                 +follow_ups_ready = false\n\
                 Apply? [Y/n] "
            )
        );
    }

    #[test]
    fn a_no_declines_it_in_the_herd_file_and_it_is_not_offered_again() {
        let (_dir, paths) = herds(LED);
        answered(&paths, Some("h"), "n\n", &[]);
        assert_eq!(
            text(&paths),
            LED.replace(
                "repo = \"/src/repo\"\n",
                "repo = \"/src/repo\"\ndeclined = [\"lead-follow-ups-ready\"]\n"
            )
        );
        assert_eq!(
            super::super::checked_repo(&paths, "h"),
            Ok("/src/repo".into())
        );
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
    }

    #[test]
    fn a_herd_that_has_follow_ups_ready_or_no_lead_is_up_to_date() {
        let has_it = LED.replace("merge = false", "follow_ups_ready = true\nmerge = false");
        let unled = &LED[..LED.find("[lead]").unwrap()];
        for herd in [has_it.as_str(), unled] {
            let (_dir, paths) = herds(herd);
            assert_eq!(
                answered(&paths, Some("h"), "", &[]),
                "herd h is up to date\n"
            );
            assert_eq!(text(&paths), herd);
        }
    }

    #[test]
    fn a_running_lead_is_told_it_reads_the_switch_at_its_next_start() {
        let (_dir, paths) = herds(LED);
        let printed = answered(&paths, Some("h"), "y\n", &["h"]);
        assert!(
            printed.contains(
                "ready for agents.\nlead-h is running; it reads this at its next restart or revive.\n---"
            ),
            "{printed}"
        );
    }

    const MERGED_BY_HAND: &str = "merged = \"GH_TOKEN=$(gh auth token --user me) gh pr list --head {branch} --state merged --json number --jq length | grep -qx 1\"\n";

    fn unled_with(merged: &str) -> String {
        let unled = &LED[..LED.find("[lead]").unwrap()];
        unled.replace(
            "done = \"exit 0\"\n",
            &format!("done = \"exit 0\"\n{merged}"),
        )
    }

    #[test]
    fn a_merged_check_naming_its_repo_is_up_to_date() {
        let herd = unled_with(
            "merged = \"gh pr list --repo o/r --head {branch} --state merged --json number --jq 'length > 0' | grep -qx true\"\n",
        );
        let (_dir, paths) = herds(&herd);
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
    }

    #[test]
    fn a_merged_check_without_its_repo_is_shown_not_changed() {
        let herd = unled_with(MERGED_BY_HAND);
        let (dir, paths) = herds(&herd);
        let printed = answered(&paths, Some("h"), "\n", &[]);
        assert_eq!(
            printed,
            "merged-repo — Names the repo in [template.issue]'s merged check with --repo <owner>/<name>,\n\
             so it works from any directory and behind an ssh host alias.\n\
             Not applied: this herd's own value is kept. It belongs in [template.issue]:\n  \
             merged = \"GH_TOKEN=$(gh auth token --user me) gh pr list --repo <owner>/<name> --head {branch} --state merged --json number --jq 'length > 0' | grep -qx true\"\n\
             Stop offering this? [y/N] "
        );
        assert_eq!(text(&paths), herd);
        assert_eq!(fs::read_dir(&paths.herds).unwrap().count(), 1, "{dir:?}");
        assert!(answered(&paths, Some("h"), "\n", &[]).starts_with("merged-repo — "));
    }

    #[test]
    fn stop_offering_a_tailored_change_declines_it() {
        let herd = unled_with(MERGED_BY_HAND);
        let (_dir, paths) = herds(&herd);
        answered(&paths, Some("h"), "y\n", &[]);
        assert_eq!(
            text(&paths),
            herd.replace(
                "repo = \"/src/repo\"\n",
                "repo = \"/src/repo\"\ndeclined = [\"merged-repo\"]\n"
            )
        );
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
    }

    #[test]
    fn the_first_write_of_a_run_backs_the_file_up_once() {
        let (_dir, paths) = herds(&LED.replace(
            "done = \"exit 0\"\n",
            &format!("done = \"exit 0\"\n{MERGED_BY_HAND}"),
        ));
        let before = text(&paths);
        answered(&paths, Some("h"), "y\ny\n", &[]);
        let backups: Vec<_> = fs::read_dir(&paths.herds)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "h.toml" && !n.starts_with('.'))
            .collect();
        assert_eq!(backups, ["h.toml.bak-2026-11-24T10:00:00Z"]);
        let kept = fs::read_to_string(paths.herds.join(&backups[0])).unwrap();
        assert_eq!(kept, before);
        assert!(text(&paths).contains("follow_ups_ready = false"));
        assert!(text(&paths).contains("declined = [\"merged-repo\"]"));
    }

    #[test]
    fn without_a_terminal_every_herd_is_listed_and_nothing_is_written() {
        let (_dir, paths) = herds(&LED.replace(
            "done = \"exit 0\"\n",
            &format!("done = \"exit 0\"\n{MERGED_BY_HAND}"),
        ));
        let unled = &LED[..LED.find("[lead]").unwrap()];
        fs::write(paths.herds.join("g.toml"), unled).unwrap();
        fs::write(paths.herds.join("k.toml"), unled_with(MERGED_BY_HAND)).unwrap();
        let before = text(&paths);
        let running_leads = BTreeSet::new();
        let mut out = vec![];
        let how = Upgrade {
            running_leads: &running_leads,
            now: at("2026-11-24T10:00:00Z"),
            terminal: None,
        };
        upgrade(&paths, None, how, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "herd g is up to date\n\
             h: 2 new features: lead-follow-ups-ready, merged-repo\n  run: loopherd herd upgrade h\n\
             k: 1 new feature: merged-repo\n  run: loopherd herd upgrade k\n"
        );
        assert_eq!(text(&paths), before);
        assert_eq!(fs::read_dir(&paths.herds).unwrap().count(), 3);
    }

    #[test]
    fn declined_ids_are_not_counted() {
        let herd = unled_with(MERGED_BY_HAND).replace(
            "repo = \"/src/repo\"\n",
            "repo = \"/src/repo\"\ndeclined = [\"merged-repo\"]\n",
        );
        let (_dir, paths) = herds(&herd);
        let running_leads = BTreeSet::new();
        let mut out = vec![];
        let how = Upgrade {
            running_leads: &running_leads,
            now: at("2026-11-24T10:00:00Z"),
            terminal: None,
        };
        upgrade(&paths, Some("h"), how, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "herd h is up to date\n");
    }

    #[test]
    fn a_merged_check_naming_its_repo_anywhere_or_not_using_gh_is_up_to_date() {
        for merged in [
            "merged = \"gh pr list --head {branch} -R o/r --state merged --json number --jq length | grep -qx 1\"\n",
            "merged = \"git merge-base --is-ancestor {branch} origin/main\"\n",
        ] {
            let (_dir, paths) = herds(&unled_with(merged));
            assert_eq!(
                answered(&paths, Some("h"), "", &[]),
                "herd h is up to date\n"
            );
        }
    }

    #[test]
    fn an_invalid_herd_is_skipped_and_the_rest_upgraded() {
        let (_dir, paths) = herds(LED);
        fs::write(
            paths.herds.join("b.toml"),
            "repo = \"/r\"\nstalled_after = 0\n",
        )
        .unwrap();
        let printed = answered(&paths, None, "\n", &[]);
        assert!(
            printed.starts_with("b: skipped, invalid b.toml: "),
            "{printed}"
        );
        assert!(text(&paths).contains("follow_ups_ready = false"));
    }

    #[test]
    fn a_lead_inheriting_follow_ups_ready_from_lead_defaults_keeps_it_on() {
        let (_dir, paths) = herds(LED);
        fs::write(
            paths.config_dir().join("config.toml"),
            "[lead_defaults]\nfollow_ups_ready = true\n",
        )
        .unwrap();
        let printed = answered(&paths, Some("h"), "\n", &[]);
        assert!(
            printed.contains("\n+follow_ups_ready = true\n"),
            "{printed}"
        );
        assert_eq!(
            text(&paths),
            LED.replace(
                "merge = false # the human merges\n",
                "merge = false # the human merges\nfollow_ups_ready = true\n"
            )
        );
        assert!(
            super::super::lead_config(&paths, "h")
                .unwrap()
                .switches
                .follow_ups_ready
        );
    }

    const LEAD_PROMPT: &str = include_str!("../init/lead.md");
    const REVIEW_ROUNDS_LINE: &str = "Your review-round limit: {review_rounds}.\n\n";
    const DRAINED_RULE: &str =
        "re-arm after every exit but `{herd} drained`: on that, report done and\n   stop.";

    /// The lead prompt init wrote before `lead-review-rounds` and `lead-drained`.
    fn old_lead_prompt() -> String {
        assert!(LEAD_PROMPT.contains(REVIEW_ROUNDS_LINE) && LEAD_PROMPT.contains(DRAINED_RULE));
        LEAD_PROMPT
            .replace(REVIEW_ROUNDS_LINE, "")
            .replace(DRAINED_RULE, "re-arm after every exit.")
    }

    /// A herd whose lead reads `prompt` from `h-lead.md` and has every other change.
    fn led_by_file(prompt: &str) -> (tempfile::TempDir, Paths) {
        let herd = LED.replace(
            "prompt = \"lead {herd} to {herd} drained in {review_rounds} rounds\"\n",
            "prompt_file = \"h-lead.md\"\nfollow_ups_ready = false\n",
        );
        let (dir, paths) = herds(&herd);
        fs::write(paths.herds.join("h-lead.md"), prompt).unwrap();
        (dir, paths)
    }

    fn lead_prompt_text(paths: &Paths) -> String {
        fs::read_to_string(paths.herds.join("h-lead.md")).unwrap()
    }

    #[test]
    fn yes_to_both_prompt_changes_gives_an_old_lead_prompt_inits_text() {
        let (_dir, paths) = led_by_file(&old_lead_prompt());
        let herd = text(&paths);
        let printed = answered(&paths, Some("h"), "y\ny\n", &[]);
        assert_eq!(lead_prompt_text(&paths), LEAD_PROMPT);
        assert_eq!(text(&paths), herd);
        let file = paths.herds.join("h-lead.md").display().to_string();
        assert!(
            printed.starts_with(&format!(
                "lead-review-rounds — Writes the lead's review-round limit into its prompt where init puts it,\n\
                 so the lead reads its {{review_rounds}} beside its switches.\n\
                 --- {file}\n+++ {file}\n"
            )),
            "{printed}"
        );
        assert!(
            printed.contains("\n+Your review-round limit: {review_rounds}.\n+\n Grilling"),
            "{printed}"
        );
        assert!(
            printed.contains(
                "-   re-arm after every exit. @ON_MERGED@ On `<name> claimable`, feed.\n\
                 +   re-arm after every exit but `{herd} drained`: on that, report done and\n\
                 +   stop. @ON_MERGED@ On `<name> claimable`, feed.\n"
            ),
            "{printed}"
        );
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
    }

    #[test]
    fn a_prompt_change_backs_up_the_prompt_file_not_the_herd_file() {
        let (_dir, paths) = led_by_file(&old_lead_prompt());
        answered(&paths, Some("h"), "y\nn\n", &[]);
        let mut files: Vec<_> = fs::read_dir(&paths.herds)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with('.'))
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                "h-lead.md",
                "h-lead.md.bak-2026-11-24T10:00:00Z",
                "h.toml",
                "h.toml.bak-2026-11-24T10:00:00Z"
            ]
        );
        let kept = fs::read_to_string(paths.herds.join(&files[1])).unwrap();
        assert_eq!(kept, old_lead_prompt());
        assert!(text(&paths).contains("declined = [\"lead-drained\"]"));
    }

    #[test]
    fn a_lead_prompt_with_each_change_by_hand_is_up_to_date() {
        let by_hand = "Lead {herd}; stop at {herd} drained.\nAt most {review_rounds} rounds.\n";
        let (_dir, paths) = led_by_file(by_hand);
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
        assert_eq!(lead_prompt_text(&paths), by_hand);
    }

    #[test]
    fn a_running_lead_is_told_it_reads_a_prompt_change_at_its_next_start() {
        let (_dir, paths) = led_by_file(&old_lead_prompt());
        let printed = answered(&paths, Some("h"), "y\ny\n", &["h"]);
        assert!(
            printed.starts_with(
                "lead-review-rounds — Writes the lead's review-round limit into its prompt where init puts it,\n\
                 so the lead reads its {review_rounds} beside its switches.\n\
                 lead-h is running; it reads this at its next restart or revive.\n---"
            ),
            "{printed}"
        );
        assert!(
            printed.contains(
                "instead of re-arming its watch after every exit.\n\
                 lead-h is running; it reads this at its next restart or revive.\n---"
            ),
            "{printed}"
        );
    }

    #[test]
    fn a_reworded_anchor_is_shown_where_it_belongs_not_changed() {
        let reworded = old_lead_prompt().replace("Grilling is always", "Grilling stays");
        let (_dir, paths) = led_by_file(&reworded);
        let file = paths.herds.join("h-lead.md").display().to_string();
        let printed = answered(&paths, Some("h"), "\nn\n", &[]);
        assert!(
            printed.starts_with(&format!(
                "lead-review-rounds — Writes the lead's review-round limit into its prompt where init puts it,\n\
                 so the lead reads its {{review_rounds}} beside its switches.\n\
                 Not applied: \"Grilling is always the human's.\" is not in {file}. \
                 It belongs in {file}, before \"Grilling is always the human's.\":\n  \
                 Your review-round limit: {{review_rounds}}.\n  \n\
                 Stop offering this? [y/N] lead-drained — "
            )),
            "{printed}"
        );
        assert_eq!(lead_prompt_text(&paths), reworded);
    }

    #[test]
    fn a_doubled_anchor_is_shown_not_changed_and_can_be_declined() {
        let doubled = old_lead_prompt().replace(
            "re-arm after every exit.",
            "re-arm after every exit. Then re-arm after every exit.",
        );
        let (_dir, paths) = led_by_file(&doubled);
        let file = paths.herds.join("h-lead.md").display().to_string();
        let printed = answered(&paths, Some("h"), "n\ny\n", &[]);
        assert!(
            printed.ends_with(&format!(
                "lead-drained — Tells the lead to report done and stop on `<herd> drained`,\n\
                 instead of re-arming its watch after every exit.\n\
                 Not applied: \"re-arm after every exit.\" appears 2 times in {file}. \
                 It belongs in {file}, in place of \"re-arm after every exit.\":\n  \
                 re-arm after every exit but `{{herd}} drained`: on that, report done and\n     \
                 stop.\n\
                 Stop offering this? [y/N] "
            )),
            "{printed}"
        );
        assert_eq!(lead_prompt_text(&paths), doubled);
        assert!(
            text(&paths).contains("declined = [\"lead-review-rounds\", \"lead-drained\"]"),
            "{}",
            text(&paths)
        );
    }

    #[test]
    fn each_prompt_migration_skips_and_tells_on_a_reworded_or_doubled_anchor() {
        let grilling = "Grilling is always the human's.";
        let re_arm = "re-arm after every exit.";
        let reworded = old_lead_prompt()
            .replace(grilling, "Grilling stays the human's.")
            .replace(re_arm, "re-arm after each exit.");
        let doubled = old_lead_prompt()
            .replace(grilling, &format!("{grilling} {grilling}"))
            .replace(re_arm, &format!("{re_arm} Then {re_arm}"));
        for (prompt, times) in [(reworded, "is not"), (doubled, "appears 2 times")] {
            let (_dir, paths) = led_by_file(&prompt);
            let file = paths.herds.join("h-lead.md").display().to_string();
            let printed = answered(&paths, Some("h"), "\n\n", &[]);
            for (id, anchor) in [("lead-review-rounds", grilling), ("lead-drained", re_arm)] {
                assert!(printed.contains(&format!("{id} — ")), "{printed}");
                assert!(
                    printed.contains(&format!("Not applied: {anchor:?} {times} in {file}.")),
                    "{id}: {printed}"
                );
            }
            assert!(!printed.contains("Apply?"), "{printed}");
            assert_eq!(lead_prompt_text(&paths), prompt);
            assert!(!text(&paths).contains("declined"), "{}", text(&paths));
        }
    }

    #[test]
    fn an_inline_lead_prompt_is_edited_in_its_own_quoting_and_layout() {
        let herd = LED.replace(
            "prompt = \"lead {herd} to {herd} drained in {review_rounds} rounds\"\n",
            "prompt = \"\"\"\nLead {herd}.\n\nGrilling is always the human's.\n\
             Watch, and re-arm after every exit.\n\"\"\" # by hand\nfollow_ups_ready = false\n",
        );
        let (_dir, paths) = herds(&herd);
        answered(&paths, Some("h"), "y\ny\n", &[]);
        assert_eq!(
            text(&paths),
            herd.replace(
                "Grilling is always",
                "Your review-round limit: {review_rounds}.\n\nGrilling is always"
            )
            .replace(
                "re-arm after every exit.",
                "re-arm after every exit but `{herd} drained`: on that, report done and\n   stop."
            )
        );
        let prompt = super::super::lead_config(&paths, "h").unwrap();
        assert!(prompt.prompt.text.contains("Your review-round limit"));
    }
}
