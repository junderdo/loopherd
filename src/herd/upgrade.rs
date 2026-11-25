//! `loopherd herd upgrade`: offer a herd the named changes newer init
//! writes that it lacks, one at a time (D157).

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Value, value};

use super::edit::edit;
use super::{Paths, SwitchValues, file_of, lead_defaults, load, names};

/// How an upgrade runs.
pub struct Upgrade<'a> {
    /// Herds whose lead is in the ledger and not retired.
    pub running_leads: &'a BTreeSet<String>,
    /// Stamps the `.bak-` copy taken before a file's first write.
    pub now: SystemTime,
    /// Where answers are read; without one nothing is applied and each herd's
    /// pending ids are listed.
    pub terminal: Option<&'a mut dyn BufRead>,
    /// Renders a herd's named template as init would write it, only when offered.
    pub render: &'a dyn Fn(&str, &str) -> Result<NewTemplate>,
}

/// A template as init would write it for one herd.
pub struct NewTemplate {
    /// Its `[template.<name>]` table as TOML, comment included.
    pub table: String,
    /// Its prompt file's name in the herds directory, and text.
    pub prompt: Option<(String, String)>,
}

/// One change newer init writes. Whether a herd has it is read from the
/// file itself, so a hand edit counts as applied.
struct Migration {
    id: &'static str,
    /// What it does and why, in at most two lines.
    explanation: &'static str,
    /// Whether a running lead only sees the change once restarted or revived.
    lead_reads_at_start: bool,
    state: fn(&DocumentMut, &SwitchValues) -> State,
}

enum State {
    /// Already there, or not for this herd.
    Current,
    Pending(Box<dyn Fn(&mut DocumentMut)>),
    /// The herd's own value sits where the change goes, so it's shown, not made.
    Tailored {
        place: &'static str,
        snippet: String,
    },
    /// A template init writes, rendered when offered.
    Template(&'static str),
}

static MIGRATIONS: [Migration; 3] = [
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
    Migration {
        id: "grill",
        explanation: "Adds [template.grill], an attended grill the shepherd starts live with the human,\n\
                      and its prompt file beside the herd file. Feed never spawns it.",
        lead_reads_at_start: false,
        state: grill,
    },
];

fn grill(doc: &DocumentMut, _: &SwitchValues) -> State {
    match doc.get("template").and_then(|t| t.get("grill")) {
        Some(_) => State::Current,
        None => State::Template("grill"),
    }
}

/// Adds `table`, a TOML `[template.<name>]`, at the end of `doc`.
fn add_template(doc: &mut DocumentMut, name: &str, table: &str) -> Result<()> {
    let mut fragment: DocumentMut = table.parse().context("rendered template")?;
    let mut new = fragment
        .get_mut("template")
        .and_then(|t| t.get_mut(name))
        .and_then(|t| t.as_table_mut())
        .map(std::mem::take)
        .with_context(|| format!("rendered template has no [template.{name}]"))?;
    let last = doc.iter().filter_map(|(_, item)| last_position(item)).max();
    new.set_position(last.map(|p| p + 1));
    // A table that follows another starts after a blank line.
    let comment = new
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    new.decor_mut().set_prefix(format!("\n{comment}"));
    let templates = doc
        .entry("template")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .context("template is not a table")?;
    templates.insert(name, Item::Table(new));
    Ok(())
}

fn last_position(item: &Item) -> Option<isize> {
    let table = item.as_table()?;
    let nested = table
        .iter()
        .filter_map(|(_, item)| last_position(item))
        .max();
    table.position().max(nested)
}

fn follow_ups_ready(doc: &DocumentMut, defaults: &SwitchValues) -> State {
    // As init writes it, so a lead inheriting the switch keeps it.
    let on = defaults.follow_ups_ready.unwrap_or(false);
    match doc.get("lead").and_then(Item::as_table_like) {
        Some(lead) if !lead.contains_key("follow_ups_ready") => {
            State::Pending(Box::new(move |doc| {
                if let Some(lead) = doc.get_mut("lead").and_then(Item::as_table_like_mut) {
                    lead.insert("follow_ups_ready", value(on));
                }
            }))
        }
        _ => State::Current,
    }
}

fn merged_repo(doc: &DocumentMut, _: &SwitchValues) -> State {
    let merged = doc
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
        place: "[template.issue]",
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
    for herd in &herds {
        if let Err(invalid) = super::checked(&file_of(&paths.herds, herd)?, paths) {
            writeln!(out, "{herd}: skipped, {invalid}")?;
            continue;
        }
        upgrade_herd(paths, herd, &mut how, out).with_context(|| format!("upgrade {herd}"))?;
    }
    Ok(())
}

/// Each valid herd's pending migration ids, declined ones left out, for
/// the herds that have any.
pub fn pending(paths: &Paths) -> Result<Vec<(String, Vec<&'static str>)>> {
    let defaults = lead_defaults(paths)?;
    let mut pending = vec![];
    for herd in names(&paths.herds)? {
        if super::checked(&file_of(&paths.herds, &herd)?, paths).is_err() {
            continue;
        }
        let ids = offered(paths, &herd, &defaults)?;
        if !ids.is_empty() {
            pending.push((herd, ids.iter().map(|m| m.id).collect()));
        }
    }
    Ok(pending)
}

/// `n new feature(s)`.
pub fn new_features(n: usize) -> String {
    match n {
        1 => "1 new feature".to_string(),
        n => format!("{n} new features"),
    }
}

fn offered(paths: &Paths, herd: &str, defaults: &SwitchValues) -> Result<Vec<&'static Migration>> {
    let declined = load(&paths.herds, herd)?.declined;
    let doc = read(&file_of(&paths.herds, herd)?)?;
    Ok(MIGRATIONS
        .iter()
        .filter(|m| !declined.iter().any(|id| id == m.id))
        .filter(|m| !matches!((m.state)(&doc, defaults), State::Current))
        .collect())
}

fn upgrade_herd(paths: &Paths, herd: &str, how: &mut Upgrade, out: &mut dyn Write) -> Result<()> {
    let file = file_of(&paths.herds, herd)?;
    let defaults = lead_defaults(paths)?;
    let pending = offered(paths, herd, &defaults)?;
    if pending.is_empty() {
        writeln!(out, "herd {herd} is up to date")?;
        return Ok(());
    }
    let Some(input) = how.terminal.as_deref_mut() else {
        let ids: Vec<_> = pending.iter().map(|m| m.id).collect();
        writeln!(
            out,
            "{herd}: {}: {}",
            new_features(ids.len()),
            ids.join(", ")
        )?;
        writeln!(out, "  run: loopherd herd upgrade {herd}")?;
        return Ok(());
    };
    let mut backup = Backup::new(&file, how.now);
    for migration in pending {
        // Read again: an earlier answer may have written the file.
        let doc = read(&file)?;
        let header = format!("{} — {}", migration.id, migration.explanation);
        let decline = match (migration.state)(&doc, &defaults) {
            State::Current => continue,
            State::Pending(change) => {
                writeln!(out, "{header}")?;
                if migration.lead_reads_at_start && how.running_leads.contains(herd) {
                    writeln!(
                        out,
                        "lead-{herd} is running; it reads this at its next restart or revive."
                    )?;
                }
                let mut after = doc.clone();
                change(&mut after);
                let (was, now) = (doc.to_string(), after.to_string());
                let shown = file.display().to_string();
                let diff = similar::TextDiff::from_lines(&was, &now);
                write!(out, "{}", diff.unified_diff().header(&shown, &shown))?;
                if ask(input, out, "Apply?", true)? {
                    backup.take()?;
                    edit(paths, herd, |doc| {
                        change(doc);
                        Ok(true)
                    })?;
                    false
                } else {
                    true
                }
            }
            State::Template(name) => {
                writeln!(out, "{header}")?;
                let offer = (how.render)(herd, name).and_then(|new| {
                    let mut after = doc.clone();
                    add_template(&mut after, name, &new.table)?;
                    Ok((new, after))
                });
                let (new, after) = match offer {
                    Ok(offer) => offer,
                    Err(e) => {
                        writeln!(out, "Not applied: can't render it: {e:#}")?;
                        if ask(input, out, "Stop offering this?", false)? {
                            backup.take()?;
                            edit(paths, herd, |doc| {
                                add_declined(doc, migration.id);
                                Ok(true)
                            })?;
                        }
                        continue;
                    }
                };
                let prompt = new
                    .prompt
                    .as_ref()
                    .map(|(name, text)| (paths.herds.join(name), text));
                if let Some((path, _)) = prompt
                    .as_ref()
                    .filter(|(path, _)| fs::symlink_metadata(path).is_ok())
                {
                    writeln!(
                        out,
                        "Not applied: {} already exists, and upgrade never replaces a prompt.",
                        path.display()
                    )?;
                    writeln!(out, "The template belongs in {}:", file.display())?;
                    for line in new.table.lines() {
                        writeln!(out, "  {line}")?;
                    }
                    ask(input, out, "Stop offering this?", false)?
                } else {
                    let (was, now) = (doc.to_string(), after.to_string());
                    let shown = file.display().to_string();
                    let diff = similar::TextDiff::from_lines(&was, &now);
                    write!(out, "{}", diff.unified_diff().header(&shown, &shown))?;
                    if let Some((path, text)) = &prompt {
                        let diff = similar::TextDiff::from_lines("", text.as_str());
                        let shown = path.display().to_string();
                        write!(out, "{}", diff.unified_diff().header("/dev/null", &shown))?;
                    }
                    if ask(input, out, "Apply?", true)? {
                        backup.take()?;
                        if let Some((path, text)) = &prompt {
                            write_new(path, text)?;
                        }
                        let added = edit(paths, herd, |doc| {
                            // Added since it was read, by hand or another run.
                            if doc.get("template").and_then(|t| t.get(name)).is_some() {
                                return Ok(false);
                            }
                            add_template(doc, name, &new.table)?;
                            Ok(true)
                        });
                        if let Err(e) = added {
                            if let Some((path, _)) = &prompt {
                                let _ = fs::remove_file(path);
                            }
                            return Err(e);
                        }
                        false
                    } else {
                        true
                    }
                }
            }
            State::Tailored { place, snippet } => {
                writeln!(out, "{header}")?;
                writeln!(
                    out,
                    "Not applied: this herd's own value is kept. It belongs in {place}:"
                )?;
                writeln!(out, "  {snippet}")?;
                ask(input, out, "Stop offering this?", false)?
            }
        };
        if decline {
            backup.take()?;
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

/// Writes `text` to a new file at `path`, refusing one that exists.
fn write_new(path: &Path, text: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    if let Err(e) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}

fn read(file: &Path) -> Result<DocumentMut> {
    let text = fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    text.parse().with_context(|| file.display().to_string())
}

/// The copy of a herd file taken once per run, before its first write.
struct Backup<'a> {
    file: &'a Path,
    now: SystemTime,
    taken: bool,
}

impl<'a> Backup<'a> {
    fn new(file: &'a Path, now: SystemTime) -> Self {
        Backup {
            file,
            now,
            taken: false,
        }
    }

    fn take(&mut self) -> Result<()> {
        if self.taken {
            return Ok(());
        }
        // edit() writes a symlink's target, so that is what is kept.
        let file =
            fs::canonicalize(self.file).with_context(|| format!("read {}", self.file.display()))?;
        let stamp = humantime::format_rfc3339_seconds(self.now);
        let mut name = file.as_os_str().to_owned();
        name.push(format!(".bak-{stamp}"));
        fs::copy(&file, &name).with_context(|| format!("back up {}", file.display()))?;
        self.taken = true;
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

[template.grill]
params = ["number"]
attended = true
prompt = "grill {number}"

[lead]
prompt = "lead {herd}"
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

    fn grill(_herd: &str, template: &str) -> Result<NewTemplate> {
        assert_eq!(template, "grill");
        Ok(NewTemplate {
            table: "# Started live.\n[template.grill]\nparams = [\"number\"]\nattended = true\n\
                    prompt_file = \"h-grill.md\"\n"
                .into(),
            prompt: Some(("h-grill.md".into(), "grill {number}\n".into())),
        })
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
            render: &grill,
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
                 @@ -20,3 +20,4 @@\n \
                 prompt = \"lead {{herd}}\"\n \
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
            render: &grill,
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
            render: &grill,
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

    const GRILLLESS: &str = "# Herd h\nrepo = \"/src/repo\"\n\n[template.issue]\nparams = [\"number\"]\n\
                             prompt = \"work {number}\"\ndone = \"exit 0\"\n";

    #[test]
    fn a_herd_without_a_grill_is_offered_one_and_a_yes_writes_the_template_and_its_prompt() {
        let (_dir, paths) = herds(GRILLLESS);
        let file = paths.herds.join("h.toml").display().to_string();
        let prompt = paths.herds.join("h-grill.md").display().to_string();

        let printed = answered(&paths, Some("h"), "\n", &[]);

        assert_eq!(
            printed,
            format!(
                "grill — Adds [template.grill], an attended grill the shepherd starts live with the human,\n\
                 and its prompt file beside the herd file. Feed never spawns it.\n\
                 --- {file}\n\
                 +++ {file}\n\
                 @@ -5,3 +5,9 @@\n \
                 params = [\"number\"]\n \
                 prompt = \"work {{number}}\"\n \
                 done = \"exit 0\"\n\
                 +\n\
                 +# Started live.\n\
                 +[template.grill]\n\
                 +params = [\"number\"]\n\
                 +attended = true\n\
                 +prompt_file = \"h-grill.md\"\n\
                 --- /dev/null\n\
                 +++ {prompt}\n\
                 @@ -0,0 +1 @@\n\
                 +grill {{number}}\n\
                 Apply? [Y/n] "
            )
        );
        assert_eq!(
            text(&paths),
            format!(
                "{GRILLLESS}\n# Started live.\n[template.grill]\nparams = [\"number\"]\n\
                 attended = true\nprompt_file = \"h-grill.md\"\n"
            )
        );
        assert_eq!(
            fs::read_to_string(paths.herds.join("h-grill.md")).unwrap(),
            "grill {number}\n"
        );
        assert_eq!(
            super::super::checked_repo(&paths, "h"),
            Ok("/src/repo".into())
        );
    }

    #[test]
    fn a_no_to_the_grill_declines_it_and_writes_no_prompt() {
        let (_dir, paths) = herds(GRILLLESS);

        answered(&paths, Some("h"), "n\n", &[]);

        assert_eq!(
            text(&paths),
            GRILLLESS.replace(
                "repo = \"/src/repo\"\n",
                "repo = \"/src/repo\"\ndeclined = [\"grill\"]\n"
            )
        );
        assert!(!paths.herds.join("h-grill.md").exists());
        assert_eq!(
            answered(&paths, Some("h"), "", &[]),
            "herd h is up to date\n"
        );
    }

    #[test]
    fn a_grill_prompt_without_its_template_is_shown_not_replaced() {
        let (_dir, paths) = herds(GRILLLESS);
        fs::write(paths.herds.join("h-grill.md"), "mine\n").unwrap();
        let file = paths.herds.join("h.toml").display().to_string();
        let prompt = paths.herds.join("h-grill.md").display().to_string();

        let printed = answered(&paths, Some("h"), "\n", &[]);

        assert!(
            printed.ends_with(&format!(
                "Not applied: {prompt} already exists, and upgrade never replaces a prompt.\n\
                 The template belongs in {file}:\n\
                 \x20 # Started live.\n  [template.grill]\n  params = [\"number\"]\n  attended = true\n  \
                 prompt_file = \"h-grill.md\"\n\
                 Stop offering this? [y/N] "
            )),
            "{printed}"
        );
        assert_eq!(text(&paths), GRILLLESS);
        assert_eq!(
            fs::read_to_string(paths.herds.join("h-grill.md")).unwrap(),
            "mine\n"
        );
    }

    fn unrenderable(paths: &Paths, answers: &str) -> String {
        let running_leads = BTreeSet::new();
        let mut input = answers.as_bytes();
        let mut out = vec![];
        let how = Upgrade {
            running_leads: &running_leads,
            now: at("2026-11-24T10:00:00Z"),
            terminal: Some(&mut input),
            render: &|_, _| bail!("no origin"),
        };
        upgrade(paths, Some("h"), how, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_grill_that_cant_be_rendered_says_why_and_a_no_offers_it_again() {
        let (_dir, paths) = herds(GRILLLESS);

        let printed = unrenderable(&paths, "\n");

        assert!(
            printed
                .ends_with("Not applied: can't render it: no origin\nStop offering this? [y/N] "),
            "{printed}"
        );
        assert_eq!(text(&paths), GRILLLESS);
        assert!(unrenderable(&paths, "\n").contains("can't render it"));
    }

    #[test]
    fn stop_offering_a_grill_that_cant_be_rendered_declines_it() {
        let (_dir, paths) = herds(GRILLLESS);

        unrenderable(&paths, "y\n");

        assert_eq!(
            text(&paths),
            GRILLLESS.replace(
                "repo = \"/src/repo\"\n",
                "repo = \"/src/repo\"\ndeclined = [\"grill\"]\n"
            )
        );
    }

    #[test]
    fn a_herd_whose_template_table_is_inline_is_told_and_the_next_herd_still_offered() {
        let (_dir, paths) = herds(GRILLLESS);
        fs::write(
            paths.herds.join("a.toml"),
            "repo = \"/r\"\ntemplate = { issue = { params = [], prompt = \"x\" } }\n",
        )
        .unwrap();

        let printed = answered(&paths, None, "\n\n", &[]);

        assert!(
            printed.contains("Not applied: can't render it: template is not a table\n"),
            "{printed}"
        );
        assert!(text(&paths).contains("[template.grill]"), "{printed}");
    }

    #[test]
    fn stop_offering_a_grill_whose_prompt_exists_declines_it() {
        let (_dir, paths) = herds(GRILLLESS);
        fs::write(paths.herds.join("h-grill.md"), "mine\n").unwrap();

        answered(&paths, Some("h"), "y\n", &[]);

        assert_eq!(
            text(&paths),
            GRILLLESS.replace(
                "repo = \"/src/repo\"\n",
                "repo = \"/src/repo\"\ndeclined = [\"grill\"]\n"
            )
        );
    }
}
