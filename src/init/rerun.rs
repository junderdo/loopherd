//! `loopherd init` in a repo whose herd exists: re-asks the settings it can
//! change in place, each changed answer shown as its own diff, then offers
//! the herd's pending migrations as `loopherd herd upgrade` does.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};
use toml_edit::{DocumentMut, Item, value};

use super::{
    GitHub, Herd, Limit, MergeGuard, Paths, Recorded, Request, account, ask, ask_context_limit,
    ask_max, checks, explain, herd, lead_defaults, merge_guard, parse_branch, rendered,
    switch_lines, tracker, write_new,
};
use crate::line_ending::LineEnding;

/// Re-runs init on `name`, the herd that runs `repo`.
pub(super) fn rerun(
    paths: &Paths,
    repo: PathBuf,
    name: String,
    req: Request,
    ask: &ask::Asker,
) -> Result<Vec<String>> {
    refuse_fixed_flags(&req)?;
    if let Err(invalid) = herd::checked_repo(paths, &name) {
        bail!("{invalid}; fix it, then run init again");
    }
    let herd_file = paths.herds.join(format!("{name}.toml"));
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let recorded = Recorded::read(&herd_file);
    let mut run = Rerun {
        paths,
        name: &name,
        ask,
        herd_file: &herd_file,
        // What init would write for the herd as it stands, to find where a
        // setting's text sits; none when the herd file doesn't say enough.
        base: recorded.as_ref().and_then(|r| base(paths, &repo, &name, r)),
        checks_as_init_writes: recorded
            .as_ref()
            .is_some_and(|r| !r.checks.is_empty() && !r.checks_reworded),
        steps: vec![],
    };
    run.checks(req.checks)?;
    run.max(req.max)?;
    let has_lead = read(&herd_file)?.contains_key("lead");
    match has_lead {
        true if req.lead == Some(false) => {
            bail!("init doesn't remove a herd's lead; drop --no-lead")
        }
        true => run.merge_guard(req.merge_guard)?,
        false => run.lead(&lead_file, req.lead, req.merge_guard)?,
    }
    run.context_limit(req.context_limit)?;
    run.account(req.gh_account)?;
    run.branch()?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let how = herd::Upgrade {
        running_leads: req.running_leads,
        now: SystemTime::now(),
        terminal: ask.terminal.then_some(&mut input as &mut dyn BufRead),
        render: &|herd, template| super::template(paths, herd, template),
    };
    let mut out: Box<dyn Write> = match ask.terminal {
        true => Box::new(std::io::stderr()),
        false => Box::new(std::io::stdout()),
    };
    herd::upgrade(paths, Some(&name), how, &mut out)?;
    let mut lines = vec![];
    if !run.steps.is_empty() {
        lines.push("Next:".into());
        for (n, step) in run.steps.iter().enumerate() {
            lines.push(format!("  {}. {step}", n + 1));
        }
    }
    Ok(lines)
}

/// Refuses the flags for what a re-run never changes.
fn refuse_fixed_flags(req: &Request) -> Result<()> {
    let fixed = [
        (req.tracker.is_some(), "--tracker"),
        (req.github.is_some(), "--repo"),
        (req.no_github, "--no-github"),
        (!req.with.is_empty(), "--with"),
    ];
    if let Some((_, flag)) = fixed.iter().find(|(given, _)| *given) {
        bail!(
            "{flag} sets up a new herd, and this repo's herd exists; \
             run loopherd herd upgrade for what newer init adds"
        );
    }
    Ok(())
}

/// The herd as today's init would settle it from what its herd file records.
fn base(paths: &Paths, repo: &Path, name: &str, recorded: &Recorded) -> Option<Herd> {
    let doc = tracker::read(&repo.join(tracker::TRACKER_DOC)).ok()?;
    let tracker = tracker::resolve(repo, doc.is_none().then_some(super::Tracker::GitHub)).ok()?;
    let github = recorded.github.as_ref().map(|slug| GitHub {
        slug: slug.clone(),
        account: recorded.account.clone().unwrap_or_default(),
        token: String::new(),
    });
    let defaults = lead_defaults(paths).ok()?;
    Some(Herd {
        repo: repo.to_path_buf(),
        name: name.to_string(),
        github,
        tracker,
        branch: recorded.branch.clone()?,
        checks: recorded.checks.clone(),
        max: recorded.max.unwrap_or(3),
        lead: recorded.lead,
        switches: switch_lines(&defaults),
        merge_guard: herd::merge_guard(paths, name).ok().flatten(),
        context_limit: recorded.context_limit,
        config_limit: String::new(),
        docs: vec![],
        agents_steps: vec![],
        groom: recorded.groom,
        grill: recorded.grill,
    })
}

struct Rerun<'a> {
    paths: &'a Paths,
    name: &'a str,
    ask: &'a ask::Asker,
    herd_file: &'a Path,
    base: Option<Herd>,
    checks_as_init_writes: bool,
    steps: Vec<String>,
}

/// One file's text now and with a change made.
struct Edit {
    path: PathBuf,
    was: String,
    now: String,
}

impl Rerun<'_> {
    fn checks(&mut self, given: &[String]) -> Result<()> {
        let probe = |herd: &mut Herd| herd.checks = vec!["loopherd-probe".into()];
        let base = match self.checks_as_init_writes {
            true => self.placeable(probe)?,
            false => None,
        };
        let Some(base) = base else {
            return self.skip(
                !given.is_empty(),
                "checks",
                &format!("[template.issue]'s prompt in {}", self.herd_file.display()),
            );
        };
        let checks = checks(given, &base.checks, self.ask)?;
        self.carry(|herd| herd.checks = checks.clone())
    }

    fn max(&mut self, given: Option<u64>) -> Result<()> {
        let doc = read(self.herd_file)?;
        let Some(current) = doc
            .get("queue")
            .and_then(|q| q.get("max"))
            .and_then(Item::as_integer)
        else {
            return self.skip(given.is_some(), "max", "[queue] max");
        };
        let max = ask_max(self.ask, given, &current.to_string())?;
        if max as i64 == current {
            return Ok(());
        }
        if self.set_key(&["queue", "max"], Some(value(max as i64)))? {
            self.update(|herd| herd.max = max);
        }
        Ok(())
    }

    /// Asked only of a lead with merge off, which `merge_guard` guards.
    fn merge_guard(&mut self, given: Option<MergeGuard>) -> Result<()> {
        let Some(current) = herd::merge_guard(self.paths, self.name)? else {
            if given.is_some() {
                bail!("--merge-guard is for a lead with merge off, and this herd's has it on");
            }
            return Ok(());
        };
        let guard = merge_guard(self.ask, given, Some(current))?;
        if guard == current {
            return Ok(());
        }
        self.switch_guard(current, guard)
    }

    /// Credentials also change every gh command's token and require sandboxed
    /// workers, so the switch is carried as init's text; back to hook, the key
    /// alone does.
    fn switch_guard(&mut self, from: MergeGuard, to: MergeGuard) -> Result<()> {
        let carried = match &self.base {
            Some(base) => {
                let from = Herd {
                    merge_guard: Some(from),
                    ..base.clone()
                };
                let to = Herd {
                    merge_guard: Some(to),
                    ..base.clone()
                };
                self.carried(&from, &to)?
            }
            None => None,
        };
        let applied = match carried {
            Some(edits) if !edits.is_empty() => self.offer(edits)?,
            _ if to == MergeGuard::Hook => {
                self.set_key(&["lead", "merge_guard"], Some(value(to.name())))?
            }
            _ => {
                eprintln!(
                    "merge_guard: not changed, as the herd's text isn't init's own; in {} set \
                     [lead] merge_guard = \"credentials\", [worker] sandbox = \"require\" and \
                     each gh command's GH_TOKEN=${{GH_TOKEN:-$(gh auth token --user <login>)}}",
                    self.herd_file.display()
                );
                false
            }
        };
        if applied {
            self.guarded(to);
        }
        Ok(())
    }

    fn lead(
        &mut self,
        lead_file: &Path,
        given: Option<bool>,
        guard: Option<MergeGuard>,
    ) -> Result<()> {
        let Some(base) = self.base.clone() else {
            return self.skip(guard.is_some(), "lead", "a [lead] table");
        };
        let question =
            "Give the herd a lead, a member that feeds, watches and verifies its workers?";
        if !self.ask.yes(question, explain::LEAD, given, false)? {
            if guard.is_some() {
                bail!(
                    "--merge-guard is for a lead, and init adds one to a herd only on a terminal"
                );
            }
            return Ok(());
        }
        if fs::symlink_metadata(lead_file).is_ok() {
            eprintln!(
                "lead: not added, as {} already exists and init never replaces a lead prompt",
                lead_file.display()
            );
            return Ok(());
        }
        let defaults = lead_defaults(self.paths)?;
        let guard = match defaults.merge {
            true if guard.is_some() => bail!(
                "--merge-guard is for a lead with merge off, and [lead_defaults] turns merge on"
            ),
            true => None,
            false => Some(merge_guard(self.ask, guard, None)?),
        };
        if guard == Some(MergeGuard::Credentials) && base.github.is_none() {
            bail!(
                "merge_guard = \"credentials\" needs the herd's repo on GitHub, \
                 and its merged check names none"
            );
        }
        // Added with hook, whose [lead] table is the only text it adds;
        // credentials is then its own change.
        let led = Herd {
            lead: true,
            merge_guard: guard.map(|_| MergeGuard::Hook),
            ..base.clone()
        };
        let (with_lead, without_lead) = (
            rendered(self.paths, &led)?.files,
            rendered(self.paths, &base)?.files,
        );
        let text_of = |files: &[(PathBuf, String)], path: &Path| {
            files
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, text)| text.clone())
        };
        let (Some(prompt), Some(with_lead), Some(without_lead)) = (
            text_of(&with_lead, lead_file),
            text_of(&with_lead, self.herd_file),
            text_of(&without_lead, self.herd_file),
        ) else {
            bail!("init rendered no lead prompt or herd file");
        };
        let diff = similar::TextDiff::from_lines(&without_lead, &with_lead);
        if diff
            .iter_all_changes()
            .any(|change| change.tag() == similar::ChangeTag::Delete)
        {
            bail!("init's [lead] table changes more of the herd file than the table");
        }
        let table: String = diff
            .iter_all_changes()
            .filter(|change| change.tag() == similar::ChangeTag::Insert)
            .map(|change| change.value())
            .collect();
        let was = fs::read_to_string(self.herd_file)?;
        let now = format!("{}\n{}", was.trim_end(), table.trim_end_matches('\n')) + "\n";
        let edits = vec![
            Edit {
                path: lead_file.to_path_buf(),
                was: String::new(),
                now: prompt,
            },
            Edit {
                path: self.herd_file.to_path_buf(),
                was,
                now,
            },
        ];
        if !self.offer(edits)? {
            return Ok(());
        }
        self.base = Some(led);
        self.steps.push(format!("loopherd lead {}", self.name));
        if guard == Some(MergeGuard::Credentials) {
            self.switch_guard(MergeGuard::Hook, MergeGuard::Credentials)?;
        }
        Ok(())
    }

    fn context_limit(&mut self, given: Option<String>) -> Result<()> {
        let doc = read(self.herd_file)?;
        let Some(issue) = doc
            .get("template")
            .and_then(|t| t.get("issue"))
            .and_then(Item::as_table_like)
        else {
            return self.skip(given.is_some(), "context_limit", "a [template.issue] table");
        };
        let current = match issue.get("context_limit") {
            None => None,
            Some(limit) => match (limit.as_integer(), limit.as_str()) {
                (Some(tokens), _) => Some(Limit::Tokens(tokens as u64)),
                (_, Some(limit)) => Some(crate::context::limit("context_limit", limit)?),
                _ => bail!(
                    "context_limit in {} is neither tokens nor a percentage",
                    self.herd_file.display()
                ),
            },
        };
        let default = current.map(|l| l.to_string()).unwrap_or_default();
        let (limit, _) = ask_context_limit(self.paths, self.ask, given, &default)?;
        if limit == current {
            return Ok(());
        }
        let limit_value = limit.map(|limit| match limit {
            Limit::Tokens(tokens) => value(tokens as i64),
            percent => value(percent.to_string()),
        });
        if self.set_key(&["template", "issue", "context_limit"], limit_value)? {
            self.update(|herd| herd.context_limit = limit);
        }
        Ok(())
    }

    fn account(&mut self, given: Option<&str>) -> Result<()> {
        if self.base.as_ref().is_some_and(|b| b.github.is_none()) {
            if given.is_some() {
                bail!("--gh-account needs a GitHub repo, and this herd's merged check names none");
            }
            return Ok(());
        }
        let probe = |herd: &mut Herd| {
            if let Some(github) = &mut herd.github {
                github.account = "loopherd-probe".into();
            }
        };
        let Some(base) = self.placeable(probe)? else {
            return self.skip(
                given.is_some(),
                "gh account",
                &format!("{} and the lead's prompt", self.herd_file.display()),
            );
        };
        if given.is_none() && !self.ask.terminal {
            return Ok(());
        }
        let current = base.github.map(|g| g.account).unwrap_or_default();
        // Checked against gh's logins only when it changes, so Enter keeps an
        // account gh has since logged out of.
        let logged_in =
            |wanted: &str| account(&current, Some(wanted), &ask::Asker { terminal: false });
        let account = match given {
            Some(given) => logged_in(given)?,
            None => self.ask.value(
                "gh account",
                explain::GH_ACCOUNT,
                None,
                Some(&current),
                "",
                |answer| match answer == current {
                    true => Ok(current.clone()),
                    false => logged_in(answer),
                },
            )?,
        };
        self.carry(|herd| {
            if let Some(github) = &mut herd.github {
                github.account = account.clone();
            }
        })
    }

    fn branch(&mut self) -> Result<()> {
        let Some(base) = self.placeable(|herd| herd.branch = "loopherd-probe".into())? else {
            return self.skip(
                false,
                "default branch",
                &format!("{} and the lead's prompt", self.herd_file.display()),
            );
        };
        let branch = self.ask.value(
            "Default branch",
            explain::DEFAULT_BRANCH,
            None,
            Some(&base.branch),
            "",
            parse_branch,
        )?;
        self.carry(|herd| herd.branch = branch.clone())
    }

    /// The base, when a change `probe` makes to it can be carried into the
    /// herd's files: the setting's text is as init writes it.
    fn placeable(&self, probe: impl Fn(&mut Herd)) -> Result<Option<Herd>> {
        let Some(base) = &self.base else {
            return Ok(None);
        };
        let mut probed = base.clone();
        probe(&mut probed);
        Ok(self.carried(base, &probed)?.map(|_| base.clone()))
    }

    /// Offers what `change` makes of init's text, carried into the herd's files.
    fn carry(&mut self, change: impl Fn(&mut Herd)) -> Result<()> {
        let Some(base) = self.base.clone() else {
            return Ok(());
        };
        let mut changed = base.clone();
        change(&mut changed);
        match self.carried(&base, &changed)? {
            Some(edits) if edits.is_empty() => {}
            Some(edits) => {
                if self.offer(edits)? {
                    self.base = Some(changed);
                }
            }
            None => eprintln!(
                "not changed: the herd's files no longer read as init wrote them; \
                 edit {} by hand",
                self.herd_file.display()
            ),
        }
        Ok(())
    }

    /// Each of the herd's files with the text init writes for `from` changed
    /// to what it writes for `to`; none when that text isn't in a file.
    fn carried(&self, from: &Herd, to: &Herd) -> Result<Option<Vec<Edit>>> {
        let mut edits = vec![];
        for ((path, was), (_, now)) in rendered(self.paths, from)?
            .files
            .iter()
            .zip(rendered(self.paths, to)?.files.iter())
        {
            if was == now {
                continue;
            }
            let actual = fs::read_to_string(path).unwrap_or_default();
            match placed(was, now, &actual) {
                Some(placed) => edits.push(Edit {
                    path: path.clone(),
                    was: actual,
                    now: placed,
                }),
                None => return Ok(None),
            }
        }
        Ok(Some(edits))
    }

    fn set_key(&mut self, keys: &[&str], new: Option<Item>) -> Result<bool> {
        let was = fs::read_to_string(self.herd_file)?;
        let mut doc: DocumentMut = was.parse()?;
        let (last, parents) = keys.split_last().expect("a key");
        let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        for key in parents {
            table = table
                .get_mut(key)
                .and_then(Item::as_table_like_mut)
                .expect("the asker checked the table exists");
        }
        match (new, table.get_mut(last).and_then(Item::as_value_mut)) {
            (Some(Item::Value(mut new)), Some(old)) => {
                *new.decor_mut() = old.decor().clone();
                *old = new;
            }
            (Some(new), _) => {
                table.insert(last, new);
            }
            (None, _) => {
                table.remove(last);
            }
        }
        let edit = Edit {
            path: self.herd_file.to_path_buf(),
            now: LineEnding::of(&was).apply(&doc.to_string()),
            was,
        };
        self.offer(vec![edit])
    }

    /// Shows each edit's diff and, on a yes on a terminal or off one, makes
    /// them: new files, then the checked herd file, undoing the new files if
    /// it is refused, then the files it changes.
    fn offer(&mut self, edits: Vec<Edit>) -> Result<bool> {
        let mut shown = String::new();
        for edit in &edits {
            let path = edit.path.display().to_string();
            let old_name = match edit.was.is_empty() {
                true => "/dev/null",
                false => &path,
            };
            let diff = similar::TextDiff::from_lines(&edit.was, &edit.now);
            shown.push_str(&diff.unified_diff().header(old_name, &path).to_string());
        }
        match self.ask.terminal {
            true => eprint!("{shown}"),
            false => print!("{shown}"),
        }
        if self.ask.terminal && !self.ask.yes("Apply?", "", None, true)? {
            return Ok(false);
        }
        let (new, changed): (Vec<_>, Vec<_>) = edits
            .into_iter()
            .partition(|edit| edit.was.is_empty() && edit.path != self.herd_file);
        let mut written: Vec<&Path> = vec![];
        let undo = |written: &[&Path]| {
            for path in written {
                let _ = fs::remove_file(path);
            }
        };
        for edit in &new {
            if let Err(e) = write_new(&edit.path, &edit.now) {
                undo(&written);
                return Err(e);
            }
            written.push(&edit.path);
        }
        let (herd_edits, prompts): (Vec<_>, Vec<_>) = changed
            .into_iter()
            .partition(|edit| edit.path == self.herd_file);
        for edit in herd_edits {
            if let Err(e) = herd::rewrite(self.paths, self.name, &edit.was, &edit.now) {
                undo(&written);
                return Err(e);
            }
        }
        for edit in prompts {
            if fs::read_to_string(&edit.path)? != edit.was {
                bail!(
                    "{} changed while init asked; run it again",
                    edit.path.display()
                );
            }
            herd::replace(&edit.path, &edit.now)?;
        }
        Ok(true)
    }

    fn update(&mut self, change: impl FnOnce(&mut Herd)) {
        if let Some(base) = &mut self.base {
            change(base);
        }
    }

    fn guarded(&mut self, guard: MergeGuard) {
        self.update(|herd| herd.merge_guard = Some(guard));
        if guard == MergeGuard::Credentials {
            self.steps.push(format!(
                "loopherd merge-guard {}, then make and store the lead's token as it says",
                self.name
            ));
        }
    }

    /// Says, on a terminal or when a flag asked for it, that `setting` isn't
    /// as init writes it and so is edited by hand at `place`.
    fn skip(&self, flagged: bool, setting: &str, place: &str) -> Result<()> {
        if self.ask.terminal || flagged {
            eprintln!(
                "{setting}: not asked, as the herd's text isn't init's own; edit it in {place}"
            );
        }
        Ok(())
    }
}

/// `actual` with each run of lines that differs between `was` and `now`
/// replaced, where that run appears in `actual` as often as in `was`; none
/// when one doesn't.
fn placed(was: &str, now: &str, actual: &str) -> Option<String> {
    let lines =
        |text: &str| -> Vec<String> { text.split_inclusive('\n').map(String::from).collect() };
    let (was_lines, now_lines, actual_lines) = (lines(was), lines(now), lines(actual));
    let mut hunks: Vec<(Vec<String>, Vec<String>)> = vec![];
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &was_lines, &now_lines) {
        let (mut old, mut new) = (op.old_range(), op.new_range());
        match op.tag() {
            similar::DiffTag::Equal => continue,
            // An insert is placed after the line before it, or before the first.
            similar::DiffTag::Insert if old.start > 0 => {
                old.start -= 1;
                new.start -= 1;
            }
            similar::DiffTag::Insert => {
                old.end += 1;
                new.end += 1;
            }
            _ => {}
        }
        hunks.push((was_lines[old].to_vec(), now_lines[new].to_vec()));
    }
    let at = |lines: &[String], block: &[String]| -> Vec<usize> {
        (0..=lines.len().saturating_sub(block.len()))
            .filter(|&i| lines.len() >= block.len() && lines[i..i + block.len()] == *block)
            .collect()
    };
    let mut replacements: Vec<(usize, usize, &[String])> = vec![];
    for (old, new) in &hunks {
        let same: Vec<_> = hunks.iter().filter(|(o, _)| o == old).collect();
        if same.iter().any(|(_, n)| n != new) {
            return None;
        }
        let found = at(&actual_lines, old);
        if found.is_empty() || found.len() != at(&was_lines, old).len() || found.len() != same.len()
        {
            return None;
        }
        for start in found {
            if !replacements.iter().any(|(s, _, _)| *s == start) {
                replacements.push((start, old.len(), new));
            }
        }
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    let mut out = String::new();
    let mut next = 0;
    for (start, len, new) in replacements {
        if start < next {
            return None;
        }
        out.extend(actual_lines[next..start].iter().cloned());
        out.extend(new.iter().cloned());
        next = start + len;
    }
    out.extend(actual_lines[next..].iter().cloned());
    Some(out)
}

fn read(file: &Path) -> Result<DocumentMut> {
    Ok(fs::read_to_string(file)?.parse()?)
}
