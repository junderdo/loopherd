//! `loopherd lead`: start a herd's lead member (D17).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::herd::{Paths, Switches, lead_config, plan};
use crate::herdr::Herdr;
use crate::ledger::{Ledger, Role};
use crate::spawn::{Checkout, MemberOf, Prompt, SHELL_TIMEOUT, SpawnRequest, Spawned, spawn};
use crate::template::Template;

pub struct LeadRequest {
    pub herd: String,
    /// Passed to claude after `[lead].args`, else the herd's.
    pub claude_args: Vec<String>,
    pub ready_timeout: Duration,
}

/// Spawns `lead-<herd>` in a detached worktree of the same name, returning
/// its name. Refuses while the herd has an unretired lead.
pub fn lead(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &LeadRequest,
    inherited_workspace: Option<&str>,
) -> Result<(String, Spawned)> {
    let spawn_req = resolve(ledger, paths, req)?;
    let spawned = spawn(herdr, ledger, &spawn_req, inherited_workspace)?;
    Ok((spawn_req.name, spawned))
}

/// `dry_run`'s plan, or `lead`'s spawn report, with the exit code.
pub fn run(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &LeadRequest,
    dry_run: bool,
    inherited_workspace: Option<&str>,
) -> Result<(String, i32)> {
    if dry_run {
        let plan = self::dry_run(herdr, ledger, paths, req, inherited_workspace)?;
        return Ok((plan, 0));
    }
    let (name, spawned) = lead(herdr, ledger, paths, req, inherited_workspace)?;
    Ok(spawned.report(&name))
}

/// What `lead` would do, like `add --dry-run` with a `role` line.
pub fn dry_run(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    paths: &Paths,
    req: &LeadRequest,
    inherited_workspace: Option<&str>,
) -> Result<String> {
    let spawn_req = resolve(ledger, paths, req)?;
    let (facts, prompt) = plan(herdr, &spawn_req, inherited_workspace)?;
    let rest = ["role lead".into(), "prompt".into(), prompt];
    Ok([facts, rest.into()].concat().join("\n"))
}

fn resolve(ledger: &dyn Ledger, paths: &Paths, req: &LeadRequest) -> Result<SpawnRequest> {
    let config = lead_config(paths, &req.herd)?;
    let name = lead_name(&req.herd)?;
    let entries = ledger.entries().context("ledger")?;
    let existing = entries
        .values()
        .find(|e| !e.retired && e.is_lead() && e.herd.as_deref() == Some(&req.herd));
    if let Some(existing) = existing {
        bail!(
            "{} already has a lead, {}; retire it first",
            req.herd,
            existing.name
        );
    }
    if entries.get(&name).is_some_and(|e| !e.retired) {
        bail!("{name} is already in the ledger as another member; retire it first");
    }
    Ok(SpawnRequest {
        repo: config.repo,
        worktree: Some(Checkout::Detached(name.clone())),
        name,
        workspace: config.workspace,
        prompt: Some(with_review_rounds(config.prompt)?),
        ready_timeout: req.ready_timeout,
        shell_timeout: SHELL_TIMEOUT,
        claude_args: [config.args, req.claude_args.clone()].concat(),
        member_of: Some(MemberOf {
            herd: req.herd.clone(),
            template: None,
            params: vec![],
            values: vec![
                ("switches".into(), config.switches.lines()),
                ("review_rounds".into(), config.review_rounds.to_string()),
            ],
            role: Some(Role::Lead),
        }),
    })
}

/// A lead's prompt as `lead` renders it, read from its herd file now.
pub struct LeadPrompt {
    pub text: String,
    pub switches: Switches,
    pub review_rounds: u32,
}

/// The prompt `lead` would start `name` in `worktree` with, so a restart can
/// give it again after `/clear` drops it.
pub fn rendered(paths: &Paths, herd: &str, name: &str, worktree: &Path) -> Result<LeadPrompt> {
    let config = lead_config(paths, herd)?;
    let prompt = with_review_rounds(config.prompt)?;
    let switches = config.switches.lines();
    let review_rounds = config.review_rounds.to_string();
    let worktree = worktree.display().to_string();
    let values = [
        ("name", name),
        ("herd", herd),
        ("worktree", worktree.as_str()),
        ("switches", switches.as_str()),
        ("review_rounds", review_rounds.as_str()),
    ];
    let text = Template::parse(&prompt.text)?
        .render(&values.into_iter().collect())
        .with_context(|| prompt.source.clone())?;
    Ok(LeadPrompt {
        text: text.trim_end().to_string(),
        switches: config.switches,
        review_rounds: config.review_rounds,
    })
}

/// `prompt`, ending with the review-round limit when it doesn't place it,
/// since the skill tells a lead the limit is in its prompt.
fn with_review_rounds(mut prompt: Prompt) -> Result<Prompt> {
    if !Template::parse(&prompt.text)?.uses("review_rounds") {
        prompt.text = format!(
            "{}\n\nYour review-round limit: {{review_rounds}}.",
            prompt.text.trim_end()
        );
    }
    Ok(prompt)
}

/// `lead-<herd>`, failing unless that is a valid member name.
fn lead_name(herd: &str) -> Result<String> {
    let valid = herd.len() <= 27
        && herd
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        bail!(
            "herd {herd:?} can't name a lead: lead-<herd> must match [a-z][a-z0-9_-]{{0,31}}, \
             so the herd name must be at most 27 of a-z, 0-9, _ and -"
        );
    }
    Ok(format!("lead-{herd}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::herd::Paths;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger, Role};
    use crate::worktree::testing;

    struct Setup {
        _root: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
    }

    impl Setup {
        /// A repo with an origin, and herd `h` with `lead` as its `[lead]` table.
        fn with_lead(lead: &str) -> Setup {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let repo = testing::repo_with_origin(&root);
            fs::create_dir(root.join("herds")).unwrap();
            let mut herd = format!(
                "repo = {:?}\nargs = [\"--model\", \"opus\"]\n\
                 [template.t]\nparams = [\"n\"]\nprompt = \"go\"\n",
                repo.display().to_string()
            );
            if !lead.is_empty() {
                herd += &format!("[lead]\n{lead}\n");
            }
            fs::write(root.join("herds/h.toml"), herd).unwrap();
            Setup {
                _root: dir,
                root,
                repo,
            }
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.root.join("herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: self.root.join("feed.lock"),
                paused: self.root.join("paused"),
            }
        }

        fn worktree(&self) -> PathBuf {
            self.root.join("proj/worktrees/lead-h")
        }

        /// Herd `name`, a copy of `h`.
        fn copy_herd(&self, name: &str) {
            let herds = self.root.join("herds");
            fs::copy(herds.join("h.toml"), herds.join(format!("{name}.toml"))).unwrap();
        }
    }

    fn request_for(herd: &str) -> LeadRequest {
        LeadRequest {
            herd: herd.into(),
            ..request(&[])
        }
    }

    fn request(claude_args: &[&str]) -> LeadRequest {
        LeadRequest {
            herd: "h".into(),
            claude_args: claude_args.iter().map(|a| a.to_string()).collect(),
            ready_timeout: Duration::from_secs(60),
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd")])
    }

    const PROMPT: &str =
        "prompt = \"{name} leads {herd} in {worktree}:\\n{switches}\"\nmerge = true";

    #[test]
    fn starts_a_lead_in_a_detached_worktree_recorded_with_its_role_and_herd() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let (name, spawned) = lead(
            &herdr,
            &mut ledger,
            &setup.paths(),
            &request(&["--effort", "max"]),
            Some("w1"),
        )
        .unwrap();

        let wt = setup.worktree();
        assert_eq!(name, "lead-h");
        assert_eq!(spawned.report(&name).1, 0);
        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(testing::current_branch(&wt), "");
        assert_eq!(
            testing::head(&wt),
            testing::head_of(&setup.repo, "origin/main")
        );
        assert_eq!(
            herdr.prompts()[0].1,
            format!(
                "lead-h leads h in {}:\nmerge on\ninstall off\nrebase off\n\
                 new_cards off\ndesign_escalation off\nblocked_dialogs off\n\n\
                 Your review-round limit: 3.",
                wt.display()
            )
        );
        let rec = &ledger.0["lead-h"];
        assert_eq!(rec.role, Some(Role::Lead));
        assert_eq!(rec.herd.as_deref(), Some("h"));
        assert_eq!(rec.template, None);
        assert_eq!(rec.worktree.as_deref(), Some(wt.as_path()));
        assert_eq!(rec.claude_args, ["--model", "opus", "--effort", "max"]);
    }

    #[test]
    fn the_leads_prompt_gets_its_review_round_limit() {
        let setup =
            Setup::with_lead("prompt = \"stop after round {review_rounds}\"\nreview_rounds = 4");
        let herdr = herdr();
        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        assert_eq!(herdr.prompts()[0].1, "stop after round 4");
    }

    #[test]
    fn a_prompt_without_the_review_round_placeholder_gets_the_limit_appended() {
        let setup = Setup::with_lead("prompt = \"go\\n\"\nreview_rounds = 4");
        let herdr = herdr();
        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        assert_eq!(herdr.prompts()[0].1, "go\n\nYour review-round limit: 4.");
    }

    #[test]
    fn the_leads_own_args_replace_the_herds() {
        let setup = Setup::with_lead("prompt = \"go\"\nargs = [\"--model\", \"sonnet\"]");
        let mut ledger = MemLedger::default();
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&["-x"]),
            Some("w1"),
        )
        .unwrap();
        assert_eq!(ledger.0["lead-h"].claude_args, ["--model", "sonnet", "-x"]);
    }

    fn lead_err(setup: &Setup, ledger: &mut MemLedger) -> String {
        let herdr = herdr();
        let err = lead(&herdr, ledger, &setup.paths(), &request(&[]), Some("w1")).unwrap_err();
        assert!(herdr.tabs().is_empty());
        format!("{err:#}")
    }

    #[test]
    fn a_herd_without_a_lead_table_is_refused() {
        let setup = Setup::with_lead("");
        assert_eq!(
            lead_err(&setup, &mut MemLedger::default()),
            "herd: h has no [lead]"
        );
        assert!(!setup.worktree().exists());
    }

    #[test]
    fn a_second_lead_for_a_herd_is_refused_until_the_first_is_retired() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let mut ledger = MemLedger::default();
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(
            lead_err(&setup, &mut ledger),
            "h already has a lead, lead-h; retire it first"
        );

        ledger.0.get_mut("lead-h").unwrap().retired = true;
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        assert!(!ledger.0["lead-h"].retired);
    }

    #[test]
    fn dry_run_prints_the_lead_and_creates_nothing() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();

        let report = dry_run(
            &herdr,
            &MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let wt = setup.worktree();
        assert_eq!(
            report,
            format!(
                "name lead-h\nbranch none\nworktree {wt}\nworkspace loopherd\n\
                 args --model opus\nrole lead\nprompt\n\
                 lead-h leads h in {wt}:\nmerge on\ninstall off\nrebase off\n\
                 new_cards off\ndesign_escalation off\nblocked_dialogs off\n\n\
                 Your review-round limit: 3.",
                wt = wt.display()
            )
        );
        assert!(!setup.worktree().exists());
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn dry_run_refuses_like_lead() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let mut ledger = MemLedger::default();
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        let err =
            dry_run(&herdr(), &ledger, &setup.paths(), &request(&[]), Some("w1")).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "h already has a lead, lead-h; retire it first"
        );
    }

    #[test]
    fn retire_close_retires_a_lead_and_removes_its_worktree() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        lead(
            &herdr,
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let mut out = vec![];
        let now = std::time::SystemTime::now();
        crate::retire::retire_and_close(
            &herdr,
            &mut ledger,
            "lead-h",
            now,
            crate::ledger::Outcome::Retired,
            std::path::Path::new("/nonexistent/reports"),
            None,
            None,
            &mut out,
        )
        .unwrap();

        assert!(ledger.0["lead-h"].retired);
        assert!(herdr.tabs().is_empty(), "{out:?}");
        assert!(!setup.worktree().exists(), "{out:?}");
    }

    #[test]
    fn leading_over_a_retired_lead_keeps_its_run_as_an_earlier_one() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let mut ledger = MemLedger::default();
        let req = request(&[]);
        lead(&herdr(), &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();
        let first = ledger.0.get_mut("lead-h").unwrap();
        first.created = "2026-11-01T00:00:00Z".into();
        first.retired = true;
        let first = first.clone();

        let (name, _) = lead(&herdr(), &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

        assert_eq!(name, "lead-h");
        let entry = &ledger.0["lead-h"];
        assert!(!entry.retired);
        assert!(entry.is_lead());
        assert_eq!(entry.earlier, [first]);
    }

    #[test]
    fn only_an_unretired_lead_of_the_same_herd_blocks_a_new_lead() {
        let setup = Setup::with_lead("prompt = \"go\"");
        setup.copy_herd("a");
        setup.copy_herd("b");
        let mut ledger = MemLedger::default();
        let paths = setup.paths();
        lead(&herdr(), &mut ledger, &paths, &request_for("a"), Some("w1")).unwrap();
        let worker = AgentRecord {
            name: "b-worker".into(),
            herd: Some("b".into()),
            role: None,
            after_merge: None,
            earlier: vec![],
            ..ledger.0["lead-a"].clone()
        };
        ledger.0.insert(worker.name.clone(), worker);

        lead(&herdr(), &mut ledger, &paths, &request_for("b"), Some("w1")).unwrap();
        assert!(ledger.0["lead-b"].is_lead());

        let err = lead(&herdr(), &mut ledger, &paths, &request_for("a"), Some("w1")).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "a already has a lead, lead-a; retire it first"
        );
    }

    #[test]
    fn an_unretired_non_lead_entry_holding_the_leads_name_is_refused_not_overwritten() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let mut ledger = MemLedger::default();
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        let squatter = AgentRecord {
            herd: None,
            role: None,
            after_merge: None,
            earlier: vec![],
            prompt: Some("a plain spawn".into()),
            ..ledger.0["lead-h"].clone()
        };
        ledger.0.insert(squatter.name.clone(), squatter.clone());

        assert_eq!(
            lead_err(&setup, &mut ledger),
            "lead-h is already in the ledger as another member; retire it first"
        );
        assert_eq!(ledger.0["lead-h"], squatter);

        ledger.0.get_mut("lead-h").unwrap().retired = true;
        lead(
            &herdr(),
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
        assert!(ledger.0["lead-h"].is_lead());
    }

    #[test]
    fn a_herd_name_that_cannot_make_a_member_name_is_refused_naming_the_rule() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let long = "h".repeat(28);
        for herd in ["Cards", long.as_str()] {
            setup.copy_herd(herd);
            let herdr = herdr();
            let err = lead(
                &herdr,
                &mut MemLedger::default(),
                &setup.paths(),
                &request_for(herd),
                Some("w1"),
            )
            .unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "herd {herd:?} can't name a lead: lead-<herd> must match \
                     [a-z][a-z0-9_-]{{0,31}}, so the herd name must be at most 27 \
                     of a-z, 0-9, _ and -"
                )
            );
            assert!(herdr.tabs().is_empty());
        }
        setup.copy_herd(&"h".repeat(27));
        lead(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &request_for(&"h".repeat(27)),
            Some("w1"),
        )
        .unwrap();
    }

    #[test]
    fn run_prints_the_plan_on_a_dry_run_and_the_spawn_report_otherwise() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let (plan, code) = run(
            &herdr,
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            true,
            Some("w1"),
        )
        .unwrap();
        assert!(plan.starts_with("name lead-h\n"), "{plan}");
        assert!(plan.contains("\nrole lead\n"), "{plan}");
        assert_eq!(code, 0);
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());

        let (report, code) = run(
            &herdr,
            &mut ledger,
            &setup.paths(),
            &request(&[]),
            false,
            Some("w1"),
        )
        .unwrap();
        assert!(report.starts_with("tab "), "{report}");
        assert_eq!(code, 0);
        assert!(ledger.0["lead-h"].is_lead());
    }
}
