//! `loopherd lead`: start a herd's lead member (D17).

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::broker::StartBroker;
use crate::herd::{Paths, Switches, lead_config, plan};
use crate::herdr::Herdr;
use crate::ledger::{Ledger, Role};
use crate::report::{self, Body, Entry};
use crate::sandbox::{self, Probe};
use crate::spawn::{Checkout, MemberOf, Prompt, SHELL_TIMEOUT, SpawnRequest, Spawned, spawn};
use crate::template::Template;
use crate::worktree::{ensure_detached_worktree, remove_worktree, worktree_path};

pub struct LeadRequest {
    pub herd: String,
    /// Passed to claude after `[lead].args`, else the herd's.
    pub claude_args: Vec<String>,
    pub ready_timeout: Duration,
    pub sandbox: Probe,
    pub broker: StartBroker,
}

/// Spawns `lead-<herd>` in a detached worktree of the same name, in its
/// sandbox, returning its name and its start's warnings: a hook-only start, or
/// what loosens its sandbox under `prefer`. Each is noted in its mailbox, and a
/// hook-only start flagged in the ledger. Refuses while the herd has an
/// unretired lead.
pub fn lead(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &LeadRequest,
    inherited_workspace: Option<&str>,
) -> Result<(String, Spawned, Vec<String>)> {
    let mut spawn_req = resolve(ledger, paths, req)?;
    let kept = sandbox::Lead {
        herd: req.herd.clone(),
        args: spawn_req.claude_args.clone(),
    };
    // Before the sandbox, so its checks read the worktree's project settings.
    let worktree = worktree_path(&spawn_req.repo, &spawn_req.name);
    let fresh = !worktree.exists();
    ensure_detached_worktree(&spawn_req.repo, &spawn_req.name).context("worktree")?;
    let launch = match sandbox::prepare(paths, &spawn_req.name, &kept, req.sandbox) {
        Ok(launch) => launch,
        // A retry would reuse the worktree as it is, not as origin then has it.
        Err(e) if fresh => {
            return Err(match remove_worktree(&spawn_req.repo, &worktree) {
                Ok(()) => e.context("sandbox"),
                Err(left) => e.context(format!(
                    "sandbox (its new worktree {} is left: {left:#})",
                    worktree.display()
                )),
            });
        }
        Err(e) => return Err(e.context("sandbox")),
    };
    spawn_req.claude_args.extend(launch.args());
    let spawned = spawn(herdr, ledger, &spawn_req, inherited_workspace)?;
    let name = spawn_req.name;
    // The lead already runs, so what can't be noted only adds to the warning.
    let mut warnings = vec![];
    for text in launch.warnings(&name) {
        let mut warning = text.clone();
        if let Some(why) = launch.hook_only() {
            let flagged = ledger.update(&name, &mut |r| {
                r.unsandboxed = Some(why.to_string());
                Ok(())
            });
            if let Err(e) = flagged {
                warning += &format!("; not flagged in the ledger: {e:#}");
            }
        }
        let note = Entry::new(SystemTime::now(), Body::Note { text });
        if let Err(e) = report::append(&paths.state_dir().join("reports"), &name, &note) {
            warning += &format!("; not noted in its mailbox: {e:#}");
        }
        warnings.push(warning);
    }
    if let Err(e) = (req.broker)(paths, &name) {
        warnings.push(format!(
            "{name}'s broker didn't start ({e:#}), so its loopherd commands fail until \
             loopherd revive --name {name} starts it"
        ));
    }
    Ok((name, spawned, warnings))
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
    let (name, spawned, warnings) = lead(herdr, ledger, paths, req, inherited_workspace)?;
    let (report, code) = spawned.report(&name);
    let warnings: String = warnings.iter().map(|w| format!("warning: {w}\n")).collect();
    Ok((warnings + &report, code))
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
        .find(|e| !e.retired && e.may_be_lead() && e.herd.as_deref() == Some(&req.herd));
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
pub fn lead_name(herd: &str) -> Result<String> {
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::herd::Paths;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger, Role};
    use crate::sandbox::Unavailable;
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
            fs::create_dir_all(root.join("config/herds")).unwrap();
            let mut herd = format!(
                "repo = {:?}\nargs = [\"--model\", \"opus\"]\n\
                 [template.t]\nparams = [\"n\"]\nprompt = \"go\"\n",
                repo.display().to_string()
            );
            if !lead.is_empty() {
                herd += &format!("[lead]\n{lead}\n");
            }
            fs::write(root.join("config/herds/h.toml"), herd).unwrap();
            Setup {
                _root: dir,
                root,
                repo,
            }
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.root.join("config/herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: self.root.join("state/feed.lock"),
                paused: self.root.join("state/paused"),
            }
        }

        fn worktree(&self) -> PathBuf {
            self.root.join("proj/worktrees/lead-h")
        }

        /// Herd `name`, a copy of `h`.
        fn copy_herd(&self, name: &str) {
            let herds = self.root.join("config/herds");
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
            sandbox: || Ok(()),
            broker: |_, _| Ok(true),
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

        let (name, spawned, _) = lead(
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
        assert_eq!(rec.claude_args[..4], ["--model", "opus", "--effort", "max"]);
    }

    /// The `--settings` file a lead's Claude was started with, parsed.
    fn started_settings(herdr: &FakeHerdr) -> (PathBuf, serde_json::Value) {
        let args = &herdr.started()[0].args;
        let at = args.iter().position(|a| a == "--settings").unwrap();
        let path = PathBuf::from(&args[at + 1]);
        let text = fs::read_to_string(&path).unwrap();
        (path, serde_json::from_str(&text).unwrap())
    }

    /// Whether an `Edit` rule in `deny` that the sandbox takes as a write deny
    /// covers `path`. The Linux sandbox skips glob rules.
    fn denied_to_sandbox(deny: &serde_json::Value, path: &Path) -> bool {
        edit_rules(deny)
            .filter(|spec| !spec.trim_end_matches("/**").contains('*'))
            .any(|spec| covers(&spec, path))
    }

    /// Whether an `Edit` rule in `deny` keeps the file tools from writing `path`.
    fn denied_to_file_tools(deny: &serde_json::Value, path: &Path) -> bool {
        edit_rules(deny).any(|spec| covers(&spec, path))
    }

    fn edit_rules(deny: &serde_json::Value) -> impl Iterator<Item = String> + '_ {
        deny.as_array().unwrap().iter().filter_map(|rule| {
            let rule = rule.as_str().unwrap();
            let spec = rule.strip_prefix("Edit(/")?.strip_suffix(')')?;
            Some(spec.to_string())
        })
    }

    /// A rule names a path and everything under it; `*` stands for one name.
    fn covers(spec: &str, path: &Path) -> bool {
        let spec = Path::new(spec.trim_end_matches("/**"));
        let path: Vec<_> = path.components().collect();
        let spec: Vec<_> = spec.components().collect();
        spec.len() <= path.len()
            && spec
                .iter()
                .zip(&path)
                .all(|(s, p)| s.as_os_str() == "*" || s == p)
    }

    #[test]
    fn a_lead_can_write_its_own_broker_requests_and_no_other_leads() {
        let setup = Setup::with_lead(PROMPT);
        setup.copy_herd("g");
        let herdr = herdr();
        let state = setup.root.join("state");
        let running = state.join("broker/lead-r/requests");
        fs::create_dir_all(&running).unwrap();
        let stray = state.join("broker/lead-h/stray");
        fs::create_dir_all(stray.parent().unwrap()).unwrap();
        fs::write(&stray, "").unwrap();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let own = state.join("broker/lead-h/requests");
        let allowed: Vec<PathBuf> = settings["sandbox"]["filesystem"]["allowWrite"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| PathBuf::from(p.as_str().unwrap()))
            .collect();
        assert!(allowed.contains(&own), "{allowed:?}");
        assert!(
            !allowed.iter().any(|dir| running.starts_with(dir)),
            "{allowed:?}"
        );
        let deny = &settings["permissions"]["deny"];
        assert!(
            !denied_to_sandbox(deny, &own.join(".1.part")),
            "an Edit deny is also a sandbox write deny, which beats allowWrite: {deny}"
        );
        // Herd g's lead hasn't started: its folder is made now, so a deny on it needs no glob.
        let unstarted = state.join("broker/lead-g");
        assert!(unstarted.is_dir());
        for path in [&running.join("x"), &unstarted.join("requests/x"), &stray] {
            assert!(denied_to_sandbox(deny, path), "{} {deny}", path.display());
        }
        let later = state.join("broker/lead-later/requests/x");
        assert!(denied_to_file_tools(deny, &later), "{deny}");
    }

    #[test]
    fn a_lead_s_writable_state_dirs_exist_when_its_claude_starts() {
        let setup = Setup::with_lead(PROMPT);
        let mut herdr = herdr();
        let state = setup.root.join("state");
        let dirs = ["broker/lead-h/requests", "handoffs/lead-h"].map(|d| state.join(d));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let record = (seen.clone(), dirs.clone());
        herdr.on_start = Some(Box::new(move || {
            let (seen, dirs) = &record;
            *seen.lock().unwrap() = dirs.iter().map(|d| d.is_dir()).collect();
        }));

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(*seen.lock().unwrap(), [true, true], "{dirs:?}");
    }

    #[test]
    fn a_lead_starts_in_the_sandbox_with_settings_written_outside_its_worktree() {
        let setup = Setup::with_lead(PROMPT);
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

        let (path, settings) = started_settings(&herdr);
        let root = setup.root.display();
        assert_eq!(path, setup.root.join("config/sandbox/lead-h.json"));
        assert_eq!(
            settings,
            serde_json::json!({
                "sandbox": {
                    "enabled": true,
                    "failIfUnavailable": true,
                    "allowUnsandboxedCommands": false,
                    "network": {"allowAllUnixSockets": true},
                    "filesystem": {
                        "allowWrite": [
                            format!("{root}/proj/worktrees/lead-h"),
                            format!("{root}/state/handoffs/lead-h"),
                            format!("{root}/state/broker/lead-h/requests"),
                        ],
                        "denyWrite": [format!("{root}/config"), "/home/me/.claude"],
                    },
                    "excludedCommands": ["loopherd watch", "loopherd watch *"],
                },
                "permissions": {
                    "denyInsteadOfPrompting": true,
                    "allow": [
                        "WebFetch(domain:*)",
                        "Bash(loopherd watch)",
                        "Bash(loopherd watch *)",
                    ],
                    "deny": [
                        format!("Edit(/{root}/config/**)"),
                        "Edit(//home/me/.claude/**)",
                        format!("Edit(/{root}/state/reports/**)"),
                        format!("Edit(/{root}/state/paused/**)"),
                        format!("Edit(/{root}/state/shepherd.json)"),
                        format!("Edit(/{root}/state/shepherd.json.lock)"),
                        format!("Edit(/{root}/state/ledger.json)"),
                        format!("Edit(/{root}/state/ledger.lock)"),
                        format!("Edit(/{root}/state/broker/*)"),
                        format!("Edit(/{root}/state/handoffs/*)"),
                        format!("Edit(/{root}/state/handoffs/shepherd/**)"),
                    ],
                },
            })
        );
        assert_eq!(
            ledger.0["lead-h"].claude_args,
            [
                "--model",
                "opus",
                "--settings",
                &path.display().to_string(),
                "--strict-mcp-config",
                "--mcp-config",
                &path.with_extension("mcp.json").display().to_string(),
            ]
        );
        assert_eq!(ledger.0["lead-h"].unsandboxed, None);
        assert_eq!(
            sandbox::Lead::kept(&setup.paths(), "lead-h").unwrap(),
            Some(sandbox::Lead {
                herd: "h".into(),
                args: vec!["--model".into(), "opus".into()],
            }),
            "kept where the lead can't write, for revive to trust over the ledger"
        );
    }

    /// The paths a lead's settings list under `key` in the sandbox's filesystem.
    fn listed(settings: &serde_json::Value, key: &str) -> Vec<PathBuf> {
        settings["sandbox"]["filesystem"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| PathBuf::from(p.as_str().unwrap()))
            .collect()
    }

    /// Starts lead-h and checks that neither its Bash nor its file tools may
    /// write `path` in the state dir: Bash because no `allowWrite` entry holds
    /// it, or because a `denyWrite` entry names it, which then exists so the
    /// sandbox leaves no placeholder there.
    fn assert_denied(path: &str, dir: bool) {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();
        let paths = setup.paths();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let full = paths.state_dir().join(path);
        let allowed = listed(&settings, "allowWrite")
            .iter()
            .any(|dir| full.starts_with(dir));
        let named = listed(&settings, "denyWrite").contains(&full);
        assert!(!allowed || named, "Bash may write {}", full.display());
        assert!(!named || full.exists(), "{}", full.display());
        let shown = full.display();
        let rule = match dir {
            true => format!("Edit(/{shown}/**)"),
            false => format!("Edit(/{shown})"),
        };
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert!(deny.contains(&rule.into()), "{deny:?}");
    }

    #[test]
    fn a_lead_cant_unpause_its_herd() {
        assert_denied("paused", true);
    }

    #[test]
    fn a_lead_cant_rewrite_the_shepherds_record() {
        assert_denied("shepherd.json", false);
    }

    #[test]
    fn a_lead_cant_hold_the_shepherds_record_lock() {
        assert_denied("shepherd.json.lock", false);
    }

    #[test]
    fn a_lead_cant_write_any_mailbox_or_watch_file() {
        assert_denied("reports", true);
    }

    #[test]
    fn a_lead_cant_rewrite_the_ledger() {
        assert_denied("ledger.json", false);
    }

    #[test]
    fn a_lead_cant_hold_the_ledger_lock() {
        assert_denied("ledger.lock", false);
    }

    #[test]
    fn a_lead_writes_in_the_state_dir_only_its_own_handoff_dir_and_broker_requests() {
        let setup = Setup::with_lead(PROMPT);
        setup.copy_herd("g");
        let herdr = herdr();
        let paths = setup.paths();
        let state = paths.state_dir().to_path_buf();
        fs::create_dir_all(state.join("handoffs/lh-9")).unwrap();
        fs::write(state.join("handoffs/lh-8.md"), "flat\n").unwrap();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let in_state: Vec<PathBuf> = listed(&settings, "allowWrite")
            .into_iter()
            .filter(|dir| dir.starts_with(&state))
            .collect();
        let own = state.join("handoffs/lead-h");
        assert_eq!(
            in_state,
            [own.clone(), state.join("broker/lead-h/requests")]
        );
        assert!(own.is_dir());
        let deny = &settings["permissions"]["deny"];
        assert!(
            !denied_to_sandbox(deny, &own.join("handoff.md")),
            "an Edit deny is also a sandbox write deny, which beats allowWrite: {deny}"
        );
        // Made now, so a deny names them without a glob.
        for dir in ["handoffs/shepherd", "handoffs/lead-g"] {
            assert!(state.join(dir).is_dir(), "{dir}");
        }
        let bash_may_write = |path: &Path| in_state.iter().any(|dir| path.starts_with(dir));
        let others = [
            "reports/lead-h.jsonl",
            "reports/lh-1.jsonl",
            "reports/watch.lock",
            "reports/.cursor",
            "handoffs/shepherd/handoff.md",
            "handoffs/shepherd.md",
            "handoffs/lead-g/handoff.md",
            "handoffs/lh-9/handoff.md",
            "handoffs/lh-8.md",
            "handoffs/lh-later/handoff.md",
        ];
        for path in others.map(|p| state.join(p)) {
            assert!(!bash_may_write(&path), "{}", path.display());
            if cfg!(target_os = "linux") {
                assert!(
                    denied_to_file_tools(deny, &path),
                    "{} {deny}",
                    path.display()
                );
            }
        }
        // What exists is named too, for a sandbox that applies globs.
        let named = [
            "reports/lh-1.jsonl",
            "handoffs/shepherd/handoff.md",
            "handoffs/lead-g/handoff.md",
            "handoffs/lh-9/handoff.md",
            "handoffs/lh-8.md",
        ];
        for path in named.map(|p| state.join(p)) {
            assert!(denied_to_sandbox(deny, &path), "{} {deny}", path.display());
        }
    }

    #[test]
    fn a_lead_restarts_prompts_retires_and_releases_through_its_broker_not_outside_the_sandbox() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let excluded = settings["sandbox"]["excludedCommands"].as_array().unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        for command in [
            "loopherd restart *",
            "loopherd prompt *",
            "loopherd retire *",
            "loopherd release *",
        ] {
            assert!(!excluded.contains(&command.into()), "{excluded:?}");
            assert!(
                !allow.contains(&format!("Bash({command})").into()),
                "{allow:?}"
            );
        }
    }

    #[test]
    fn a_leads_watch_runs_outside_the_sandbox_in_its_plain_forms_only() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let excluded = settings["sandbox"]["excludedCommands"].as_array().unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        for command in ["loopherd watch", "loopherd watch *"] {
            assert!(excluded.contains(&command.into()), "{excluded:?}");
            assert!(
                allow.contains(&format!("Bash({command})").into()),
                "an excluded command still goes through the permission flow: {allow:?}"
            );
        }
        assert_eq!(excluded.len(), 2, "{excluded:?}");
    }

    /// The `--mcp-config` a lead's Claude was started with, parsed, after `--strict-mcp-config`.
    fn started_mcp(herdr: &FakeHerdr) -> (PathBuf, serde_json::Value) {
        let args = &herdr.started()[0].args;
        let at = args.iter().position(|a| a == "--mcp-config").unwrap();
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "{args:?}"
        );
        let path = PathBuf::from(&args[at + 1]);
        let text = fs::read_to_string(&path).unwrap();
        (path, serde_json::from_str(&text).unwrap())
    }

    #[test]
    fn a_lead_starts_with_no_mcp_servers_from_any_scope() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (path, mcp) = started_mcp(&herdr);
        assert_eq!(path, setup.root.join("config/sandbox/lead-h.mcp.json"));
        assert_eq!(mcp, serde_json::json!({"mcpServers": {}}));
    }

    #[test]
    fn a_lead_on_prefer_without_its_sandbox_still_starts_with_no_mcp_servers() {
        let setup = Setup::with_lead("prompt = \"go\"\nsandbox = \"prefer\"");
        let herdr = herdr();
        let req = LeadRequest {
            sandbox: || {
                Err(Unavailable {
                    why: "bubblewrap not installed".into(),
                    fix: "install bubblewrap".into(),
                })
            },
            ..request(&[])
        };

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &req,
            Some("w1"),
        )
        .unwrap();

        assert_eq!(started_mcp(&herdr).1, serde_json::json!({"mcpServers": {}}));
    }

    #[test]
    fn a_lead_gets_the_mcp_servers_its_herd_names_from_the_humans_user_scope_only() {
        let setup = Setup::with_lead("prompt = \"go\"\nmcp_servers = [\"docs\", \"browser\"]");
        let herdr = herdr();
        let home = setup.root.join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = setup.repo.display().to_string();
        fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": {
                    "docs": {"type": "http", "url": "https://docs.example/mcp"},
                    "browser": {"command": "browser-mcp"},
                    "mail": {"command": "mail-mcp"},
                },
                "projects": {repo: {"mcpServers": {"browser": {"command": "planted"}}}},
            })
            .to_string(),
        )
        .unwrap();
        let paths = Paths {
            home,
            ..setup.paths()
        };

        lead(
            &herdr,
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(
            started_mcp(&herdr).1,
            serde_json::json!({"mcpServers": {
                "docs": {"type": "http", "url": "https://docs.example/mcp"},
                "browser": {"command": "browser-mcp"},
            }})
        );
    }

    #[test]
    fn a_lead_naming_an_mcp_server_the_human_hasnt_configured_is_refused() {
        let setup = Setup::with_lead("prompt = \"go\"\nmcp_servers = [\"docs\"]");
        let herdr = herdr();

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "sandbox: lead-h can't start: herd h's [lead] mcp_servers names \"docs\", which \
             /home/me/.claude.json doesn't configure in your user scope; add it with claude mcp \
             add --scope user, or drop it from mcp_servers (local and project scopes, claude.ai \
             connectors and plugins' servers can't be given to a lead)"
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn a_lead_given_its_own_mcp_config_is_refused_since_its_launch_takes_that_flag() {
        let setup = Setup::with_lead(PROMPT);
        for flag in ["--mcp-config", "--strict-mcp-config", "--mcp-config=x.json"] {
            let herdr = herdr();

            let err = lead(
                &herdr,
                &mut MemLedger::default(),
                &setup.paths(),
                &request(&[flag]),
                Some("w1"),
            )
            .unwrap_err();

            assert!(
                format!("{err:#}").starts_with(&format!(
                    "sandbox: lead-h's args pass {}, which its launch takes;",
                    flag.split('=').next().unwrap()
                )),
                "{err:#}"
            );
        }
    }

    #[test]
    fn a_lead_given_its_own_settings_is_refused_since_its_sandbox_takes_that_flag() {
        let setup = Setup::with_lead(PROMPT);
        let herdr = herdr();

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&["--settings", "/mine.json"]),
            Some("w1"),
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").starts_with(
                "sandbox: lead-h's args pass --settings, which its sandbox's settings file takes;"
            ),
            "{err:#}"
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn install_on_lets_the_lead_write_the_binary_and_run_the_installer_outside_the_sandbox() {
        let setup = Setup::with_lead("prompt = \"go\"\ninstall = true");
        let herdr = herdr();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (_, settings) = started_settings(&herdr);
        let sandbox = &settings["sandbox"];
        assert_eq!(
            sandbox["filesystem"]["allowWrite"][3],
            "/home/me/.local/bin"
        );
        assert_eq!(
            sandbox["excludedCommands"],
            serde_json::json!([
                "loopherd watch",
                "loopherd watch *",
                "scripts/install",
                "scripts/install *",
                "./scripts/install",
                "./scripts/install *",
            ])
        );
        assert_eq!(
            settings["permissions"]["allow"][3], "Bash(scripts/install)",
            "an excluded command still goes through the permission flow"
        );
        assert_eq!(
            sandbox["filesystem"]["denyWrite"][1], "/home/me/.claude",
            "the file tools stay out of ~/.claude"
        );
    }

    const CREDENTIALS: &str = "prompt = \"go\"\nmerge_guard = \"credentials\"";

    /// Points origin at GitHub by name while git still fetches the local one.
    fn origin_on_github(setup: &Setup, url: &str) {
        let local = setup.root.join("origin.git").display().to_string();
        testing::git(&setup.repo, &["remote", "set-url", "origin", url]).unwrap();
        let rewrite = format!("url.{local}.insteadOf");
        testing::git(&setup.repo, &["config", &rewrite, url]).unwrap();
    }

    fn store_credentials(setup: &Setup, key: bool, token: Option<&str>) {
        let dir = setup.root.join("config/credentials");
        fs::create_dir_all(&dir).unwrap();
        if key {
            fs::write(dir.join("h.key"), "a private key\n").unwrap();
        }
        if let Some(token) = token {
            fs::write(dir.join("h.token"), token).unwrap();
        }
    }

    #[test]
    fn a_credentials_lead_gets_its_deploy_key_and_token_and_cant_read_the_humans() {
        let setup = Setup::with_lead(CREDENTIALS);
        origin_on_github(&setup, "https://github.com/acme/widgets.git");
        store_credentials(&setup, true, Some("github_pat_1\n"));
        let herdr = herdr();

        lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();

        let (path, settings) = started_settings(&herdr);
        let fetch = "url.git@loopherd-h:acme/widgets.git.insteadOf";
        let push = "url.git@loopherd-h:acme/widgets.git.pushInsteadOf";
        assert_eq!(
            settings["env"],
            serde_json::json!({
                "GH_TOKEN": "github_pat_1",
                "SSH_AUTH_SOCK": "",
                "GIT_CONFIG_COUNT": "5",
                "GIT_CONFIG_KEY_0": "credential.helper",
                "GIT_CONFIG_VALUE_0": "",
                "GIT_CONFIG_KEY_1": fetch,
                "GIT_CONFIG_VALUE_1": "https://github.com/acme/widgets.git",
                "GIT_CONFIG_KEY_2": push,
                "GIT_CONFIG_VALUE_2": "https://github.com/acme/widgets.git",
                "GIT_CONFIG_KEY_3": fetch,
                "GIT_CONFIG_VALUE_3": "git@github.com:acme/widgets.git",
                "GIT_CONFIG_KEY_4": push,
                "GIT_CONFIG_VALUE_4": "git@github.com:acme/widgets.git",
            })
        );
        assert_eq!(
            settings["sandbox"]["filesystem"]["allowRead"],
            serde_json::json!(["/home/me/.ssh/config", "/home/me/.ssh/known_hosts"])
        );
        assert_eq!(
            settings["sandbox"]["filesystem"]["denyRead"],
            serde_json::json!([
                "/home/me/.ssh",
                "/home/me/.netrc",
                "/home/me/.config/gh",
                "/home/me/.git-credentials",
                "/home/me/.config/git/credentials",
                "/home/me/.cache/git/credential",
            ])
        );
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        assert_eq!(
            deny[deny.len() - 6..],
            serde_json::json!([
                "Read(//home/me/.ssh/**)",
                "Read(//home/me/.netrc)",
                "Read(//home/me/.config/gh/**)",
                "Read(//home/me/.git-credentials)",
                "Read(//home/me/.config/git/credentials)",
                "Read(//home/me/.cache/git/credential/**)",
            ])
            .as_array()
            .unwrap()[..]
        );
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the settings hold the token");
    }

    #[test]
    fn a_hook_or_merging_lead_keeps_the_humans_credentials() {
        for lead_table in [
            "prompt = \"go\"",
            "prompt = \"go\"\nmerge = true\nmerge_guard = \"credentials\"",
        ] {
            let setup = Setup::with_lead(lead_table);
            let herdr = herdr();

            lead(
                &herdr,
                &mut MemLedger::default(),
                &setup.paths(),
                &request(&[]),
                Some("w1"),
            )
            .unwrap();

            let (_, settings) = started_settings(&herdr);
            assert_eq!(settings.get("env"), None, "{lead_table}");
            assert_eq!(
                settings["sandbox"]["filesystem"].get("denyRead"),
                None,
                "{lead_table}"
            );
            let deny = settings["permissions"]["deny"].to_string();
            assert!(!deny.contains("Read("), "{lead_table}: {deny}");
        }
    }

    #[test]
    fn a_credentials_lead_without_its_key_or_token_isnt_started_and_is_told_the_fix() {
        let config = |setup: &Setup, file: &str| {
            setup
                .root
                .join("config/credentials")
                .join(file)
                .display()
                .to_string()
        };
        let no_key = Setup::with_lead(CREDENTIALS);
        store_credentials(&no_key, false, Some("github_pat_1"));
        let no_token = Setup::with_lead(CREDENTIALS);
        store_credentials(&no_token, true, None);
        let empty_token = Setup::with_lead(CREDENTIALS);
        store_credentials(&empty_token, true, Some("\n"));
        let cases = [
            (
                &no_key,
                format!(
                    "sandbox: lead-h can't start with merge_guard = \"credentials\": no deploy \
                     key at {}; run loopherd merge-guard h",
                    config(&no_key, "h.key")
                ),
            ),
            (
                &no_token,
                format!(
                    "sandbox: lead-h can't start with merge_guard = \"credentials\": no token \
                     at {}; run loopherd merge-guard h for how to create and store it",
                    config(&no_token, "h.token")
                ),
            ),
            (
                &empty_token,
                format!(
                    "sandbox: lead-h can't start with merge_guard = \"credentials\": no token \
                     at {}; run loopherd merge-guard h for how to create and store it",
                    config(&empty_token, "h.token")
                ),
            ),
        ];
        for (setup, refusal) in cases {
            origin_on_github(setup, "https://github.com/acme/widgets.git");
            let herdr = herdr();

            let err = lead(
                &herdr,
                &mut MemLedger::default(),
                &setup.paths(),
                &request(&[]),
                Some("w1"),
            )
            .unwrap_err();

            assert_eq!(format!("{err:#}"), refusal);
            assert!(herdr.tabs().is_empty());
        }
    }

    #[test]
    fn a_credentials_lead_isnt_started_without_its_sandbox_even_under_prefer() {
        let setup = Setup::with_lead(&format!("{CREDENTIALS}\nsandbox = \"prefer\""));
        origin_on_github(&setup, "https://github.com/acme/widgets.git");
        store_credentials(&setup, true, Some("github_pat_1"));
        let herdr = herdr();
        let req = LeadRequest {
            sandbox: || {
                Err(Unavailable {
                    why: "bubblewrap not installed".into(),
                    fix: "install bubblewrap".into(),
                })
            },
            ..request(&[])
        };

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &req,
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "sandbox: lead-h can't start with merge_guard = \"credentials\": its sandbox \
             can't start (bubblewrap not installed), and without it the lead could read your \
             GitHub credentials; install bubblewrap, or set [lead] merge_guard = \"hook\" in \
             herd h"
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn a_credentials_lead_that_may_install_isnt_started() {
        let setup = Setup::with_lead(&format!("{CREDENTIALS}\ninstall = true"));
        origin_on_github(&setup, "https://github.com/acme/widgets.git");
        store_credentials(&setup, true, Some("github_pat_1"));
        let herdr = herdr();

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "sandbox: lead-h can't start with merge_guard = \"credentials\": install = true \
             runs scripts/install outside the sandbox, from a worktree the lead can edit; set \
             [lead] install = false, or merge_guard = \"hook\", in herd h"
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn require_refuses_a_lead_whose_sandbox_cant_start_and_says_why() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let herdr = herdr();
        let req = LeadRequest {
            sandbox: || {
                Err(Unavailable {
                    why: "bubblewrap not installed".into(),
                    fix: "install bubblewrap".into(),
                })
            },
            ..request(&[])
        };

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &req,
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "sandbox: lead-h can't run sandboxed: its sandbox can't start: bubblewrap not \
             installed; install bubblewrap, or set [lead] sandbox = \"prefer\" in herd h \
             to run it guarded by its tool hook alone"
        );
        assert!(herdr.tabs().is_empty());
    }

    /// `setup`'s paths with a home whose `~/.claude/settings.json` is `settings`.
    fn home_with_settings(setup: &Setup, settings: &str) -> Paths {
        let home = setup.root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), settings).unwrap();
        Paths {
            home,
            ..setup.paths()
        }
    }

    #[test]
    fn require_refuses_a_lead_whose_user_settings_loosen_its_sandbox_naming_each_key() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let herdr = herdr();
        let paths = home_with_settings(
            &setup,
            r#"{"sandbox": {"excludedCommands": ["docker *"],
                "filesystem": {"allowWrite": ["/srv", "~/notes"], "disabled": true}}}"#,
        );

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let file = setup.root.join("home/.claude/settings.json");
        let file = file.display();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't run sandboxed: your Claude settings loosen its sandbox: \
                 {file}: sandbox.excludedCommands runs \"docker *\" outside it; \
                 {file}: sandbox.filesystem.allowWrite opens /srv to writes; \
                 {file}: sandbox.filesystem.allowWrite opens ~/notes to writes; \
                 {file}: sandbox.filesystem.disabled turns its filesystem rules off; \
                 remove those, or set [lead] sandbox = \"prefer\" in herd h to start it with \
                 them as warnings"
            )
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn prefer_starts_a_lead_in_its_loosened_sandbox_warning_of_each_key() {
        let setup = Setup::with_lead("prompt = \"go\"\nsandbox = \"prefer\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let paths =
            home_with_settings(&setup, r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);

        let (out, code) = run(
            &herdr,
            &mut ledger,
            &paths,
            &request(&[]),
            false,
            Some("w1"),
        )
        .unwrap();

        let file = setup.root.join("home/.claude/settings.json");
        let warning = format!(
            "lead-h's sandbox is loosened by {}: sandbox.excludedCommands runs \"docker *\" \
             outside it",
            file.display()
        );
        assert_eq!(code, 0);
        assert!(out.starts_with(&format!("warning: {warning}\n")), "{out}");
        assert!(herdr.started()[0].args.contains(&"--settings".to_string()));
        assert_eq!(ledger.0["lead-h"].unsandboxed, None);
        let (mailbox, _) = report::history(&setup.root.join("state/reports"), "lead-h").unwrap();
        assert!(
            mailbox.iter().any(|e| e.body
                == Body::Note {
                    text: warning.clone()
                }),
            "{mailbox:?}"
        );
    }

    #[test]
    fn user_settings_that_the_leads_own_settings_already_set_dont_count() {
        let setup = Setup::with_lead("prompt = \"go\"\ninstall = true");
        let worktree = setup.worktree();
        let paths = home_with_settings(
            &setup,
            &format!(
                r#"{{"sandbox": {{"enabled": false, "allowUnsandboxedCommands": true,
                    "excludedCommands": ["scripts/install *"],
                    "enableWeakerNestedSandbox": false,
                    "filesystem": {{"allowWrite": [{worktree:?}], "allowRead": ["/srv"]}}}},
                    "permissions": {{"allow": ["Bash(*)", "Read(//srv/**)"]}}}}"#
            ),
        );

        let started = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        );

        assert!(
            started.as_ref().is_ok_and(|(_, _, w)| w.is_empty()),
            "{started:?}"
        );
    }

    #[test]
    fn user_permissions_that_open_writes_count_even_beneath_what_the_lead_may_write() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let beneath = setup.worktree().join("link").display().to_string();
        let paths = home_with_settings(
            &setup,
            &format!(
                r#"{{"sandbox": {{"filesystem": {{"allowWrite": [{beneath:?}]}}}},
                    "permissions": {{"additionalDirectories": ["~/notes"],
                    "allow": ["Edit(//srv/**)", "Edit"]}}}}"#
            ),
        );

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let file = setup.root.join("home/.claude/settings.json");
        let file = file.display();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't run sandboxed: your Claude settings loosen its sandbox: \
                 {file}: sandbox.filesystem.allowWrite opens {beneath} to writes; \
                 {file}: permissions.additionalDirectories opens ~/notes to writes; \
                 {file}: permissions.allow opens Edit(//srv/**) to writes; \
                 {file}: permissions.allow opens Edit to writes; \
                 remove those, or set [lead] sandbox = \"prefer\" in herd h to start it with \
                 them as warnings"
            )
        );
    }

    #[test]
    fn a_credentials_lead_whose_user_settings_loosen_its_sandbox_isnt_started_even_under_prefer() {
        let setup = Setup::with_lead(&format!("{CREDENTIALS}\nsandbox = \"prefer\""));
        origin_on_github(&setup, "https://github.com/acme/widgets.git");
        store_credentials(&setup, true, Some("github_pat_1"));
        let paths =
            home_with_settings(&setup, r#"{"sandbox": {"excludedCommands": ["docker *"]}}"#);

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't start with merge_guard = \"credentials\": your Claude \
                 settings loosen its sandbox: {}: sandbox.excludedCommands runs \"docker *\" \
                 outside it; remove those, or set [lead] merge_guard = \"hook\" in herd h",
                setup.root.join("home/.claude/settings.json").display()
            )
        );
    }

    #[test]
    fn unparseable_user_settings_count_as_a_loosening() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let paths = home_with_settings(&setup, "{ not json");

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let file = setup.root.join("home/.claude/settings.json");
        let shown = format!("{err:#}");
        assert!(
            shown.contains(&format!("{}: can't parse it: ", file.display())),
            "{shown}"
        );
    }

    #[test]
    fn unreadable_user_settings_count_as_a_loosening() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let paths = Paths {
            home: setup.root.join("home"),
            ..setup.paths()
        };
        let file = setup.root.join("home/.claude/settings.json");
        fs::create_dir_all(&file).unwrap();

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let shown = format!("{err:#}");
        assert!(
            shown.contains(&format!("{}: can't read it: ", file.display())),
            "{shown}"
        );
    }

    #[test]
    fn user_rules_that_write_anywhere_a_bypass_default_and_network_keys_count() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let paths = home_with_settings(
            &setup,
            r#"{"permissions": {"allow": ["Write", "NotebookEdit(//srv/**)", "MultiEdit(*)",
                "Read(//srv/**)", "Edit(//**)", "Edit(//*)", "Edit(~/notes/*.md)"],
                "defaultMode": "bypassPermissions"},
                "sandbox": {"network": {"httpProxyPort": 8080, "socksProxyPort": 1080,
                "allowLocalBinding": true, "allowMachLookup": ["com.apple.x"]}}}"#,
        );

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &paths,
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let file = setup.root.join("home/.claude/settings.json");
        let file = file.display();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't run sandboxed: your Claude settings loosen its sandbox: \
                 {file}: sandbox.network.httpProxyPort hands its network to your proxy, past \
                 its own checks on local addresses; \
                 {file}: sandbox.network.socksProxyPort hands its network to your proxy, past \
                 its own checks on local addresses; \
                 {file}: sandbox.network.allowLocalBinding lets it bind local ports on macOS; \
                 {file}: sandbox.network.allowMachLookup opens macOS services to it; \
                 {file}: permissions.allow opens Write to writes; \
                 {file}: permissions.allow opens MultiEdit(*) to writes; \
                 {file}: permissions.allow opens Edit(//**) to writes; \
                 {file}: permissions.allow opens Edit(//*) to writes; \
                 {file}: permissions.allow opens Edit(~/notes/*.md) to writes; \
                 {file}: permissions.defaultMode bypassPermissions lets its file tools write \
                 anywhere; \
                 remove those, or set [lead] sandbox = \"prefer\" in herd h to start it with \
                 them as warnings"
            )
        );
    }

    #[test]
    fn project_rules_that_reach_past_a_new_lead_worktree_count_and_its_worktree_is_removed() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let seed = setup.root.join("seed");
        fs::create_dir_all(seed.join(".claude")).unwrap();
        fs::write(
            seed.join(".claude/settings.json"),
            r#"{"permissions": {"additionalDirectories": ["~/.local/bin"],
                "allow": ["Edit(/src/**)", "Edit(/)", "Edit(src/**/*.rs)", "Edit(//**)",
                "Edit(/../x/**)", "Write(//srv/**)"]},
                "sandbox": {"excludedCommands": ["docker *"]}}"#,
        )
        .unwrap();
        testing::git_in(&seed, &["add", ".claude"]);
        testing::commit(&seed, "project settings");
        let origin = setup.root.join("origin.git");
        testing::git_in(
            &seed,
            &["push", "--quiet", origin.to_str().unwrap(), "main"],
        );
        let herdr = herdr();

        let err = lead(
            &herdr,
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let file = setup.worktree().join(".claude/settings.json");
        let file = file.display();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't run sandboxed: your Claude settings loosen its sandbox: \
                 {file}: permissions.additionalDirectories opens ~/.local/bin to writes; \
                 {file}: permissions.allow opens Edit(//**) to writes; \
                 {file}: permissions.allow opens Edit(/../x/**) to writes; \
                 remove those, or set [lead] sandbox = \"prefer\" in herd h to start it with \
                 them as warnings"
            )
        );
        assert!(herdr.tabs().is_empty());
        assert!(
            !setup.worktree().exists(),
            "a refused lead's new worktree is removed"
        );
    }

    #[test]
    fn the_local_settings_in_a_kept_lead_worktree_count_for_their_permissions() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let worktree = ensure_detached_worktree(&setup.repo, "lead-h").unwrap();
        fs::create_dir_all(worktree.join(".claude")).unwrap();
        let file = worktree.join(".claude/settings.local.json");
        fs::write(&file, r#"{"permissions": {"allow": ["Edit(//*)"]}}"#).unwrap();

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!(
                "sandbox: lead-h can't run sandboxed: your Claude settings loosen its sandbox: \
                 {}: permissions.allow opens Edit(//*) to writes; remove those, or set \
                 [lead] sandbox = \"prefer\" in herd h to start it with them as warnings",
                file.display()
            )
        );
        assert!(file.exists(), "a kept worktree stays");
    }

    #[test]
    fn empty_placeholder_settings_in_a_kept_lead_worktree_count_as_none() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let worktree = ensure_detached_worktree(&setup.repo, "lead-h").unwrap();
        fs::create_dir_all(worktree.join(".claude")).unwrap();
        for name in ["settings.json", "settings.local.json"] {
            fs::write(worktree.join(".claude").join(name), "").unwrap();
        }

        lead(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap();
    }

    #[test]
    fn malformed_settings_in_a_kept_lead_worktree_still_refuse_under_require() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let worktree = ensure_detached_worktree(&setup.repo, "lead-h").unwrap();
        fs::create_dir_all(worktree.join(".claude")).unwrap();
        let file = worktree.join(".claude/settings.json");
        fs::write(&file, "{ not json").unwrap();

        let err = lead(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &request(&[]),
            Some("w1"),
        )
        .unwrap_err();

        let shown = format!("{err:#}");
        assert!(
            shown.contains(&format!("{}: can't parse it: ", file.display())),
            "{shown}"
        );
    }

    #[test]
    fn prefer_starts_a_lead_whose_sandbox_cant_start_with_a_warning_in_its_mailbox() {
        let setup = Setup::with_lead("prompt = \"go\"\nsandbox = \"prefer\"");
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let req = LeadRequest {
            sandbox: || {
                Err(Unavailable {
                    why: "bubblewrap not installed".into(),
                    fix: "install bubblewrap".into(),
                })
            },
            ..request(&[])
        };

        let (out, code) =
            run(&herdr, &mut ledger, &setup.paths(), &req, false, Some("w1")).unwrap();

        let warning = "lead-h runs without its sandbox (bubblewrap not installed), \
                       guarded by its tool hook alone";
        assert_eq!(code, 0);
        assert!(out.starts_with(&format!("warning: {warning}\n")), "{out}");
        assert!(!herdr.started()[0].args.contains(&"--settings".to_string()));
        assert_eq!(
            ledger.0["lead-h"].unsandboxed.as_deref(),
            Some("bubblewrap not installed")
        );
        let (mailbox, _) = report::history(&setup.root.join("state/reports"), "lead-h").unwrap();
        assert!(
            mailbox.iter().any(|e| e.body
                == Body::Note {
                    text: warning.into()
                }),
            "{mailbox:?}"
        );
    }

    #[test]
    fn a_started_lead_gets_its_broker_started() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let req = LeadRequest {
            broker: |paths, lead| {
                let dir = paths.state_dir();
                fs::create_dir_all(dir)?;
                fs::write(dir.join("broker-started"), lead)?;
                Ok(true)
            },
            ..request(&[])
        };

        let (_, code) = run(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &req,
            false,
            Some("w1"),
        )
        .unwrap();

        assert_eq!(code, 0);
        let started = fs::read_to_string(setup.root.join("state/broker-started")).unwrap();
        assert_eq!(started, "lead-h");
    }

    #[test]
    fn a_broker_that_does_not_start_is_a_warning_naming_revive() {
        let setup = Setup::with_lead("prompt = \"go\"");
        let req = LeadRequest {
            broker: |_, _| anyhow::bail!("no binary"),
            ..request(&[])
        };

        let (out, code) = run(
            &herdr(),
            &mut MemLedger::default(),
            &setup.paths(),
            &req,
            false,
            Some("w1"),
        )
        .unwrap();

        assert_eq!(code, 0);
        assert!(
            out.starts_with(
                "warning: lead-h's broker didn't start (no binary), so its loopherd commands \
                 fail until loopherd revive --name lead-h starts it\n"
            ),
            "{out}"
        );
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
        assert_eq!(
            ledger.0["lead-h"].claude_args[..3],
            ["--model", "sonnet", "-x"]
        );
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
    fn a_lead_is_refused_while_a_member_with_a_role_from_another_version_holds_the_herd() {
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
        ledger.0.get_mut("lead-h").unwrap().role =
            Some(crate::ledger::Role::Unknown("reviewer".into()));

        assert_eq!(
            lead_err(&setup, &mut ledger),
            "h already has a lead, lead-h; retire it first"
        );
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
            &crate::sandbox::Guard::new(setup.paths(), None),
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

        let (name, _, _) = lead(&herdr(), &mut ledger, &setup.paths(), &req, Some("w1")).unwrap();

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
            unsandboxed: None,
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
            unsandboxed: None,
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
