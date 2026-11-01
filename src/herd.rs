//! Herd files (D7, D9) and `loopherd add`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::spawn::{MemberOf, SpawnRequest, Spawned, preview, spawn};
use crate::template::Template;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HerdFile {
    repo: String,
    workspace: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    template: BTreeMap<String, TemplateFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    params: Vec<String>,
    name: Option<String>,
    branch: Option<String>,
    prompt: Option<String>,
    prompt_file: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

/// Where herd files live, and the home a leading `~` expands to.
pub struct Paths {
    pub herds: PathBuf,
    pub home: PathBuf,
}

impl Paths {
    /// `$LOOPHERD_CONFIG_DIR/herds`, else `$XDG_CONFIG_HOME/loopherd/herds`,
    /// defaulting to `~/.config/loopherd/herds`.
    pub fn from_env() -> Result<Paths> {
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let home = PathBuf::from(var("HOME").context("HOME is not set")?);
        let config = match (var("LOOPHERD_CONFIG_DIR"), var("XDG_CONFIG_HOME")) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(xdg)) => PathBuf::from(xdg).join("loopherd"),
            (None, None) => home.join(".config/loopherd"),
        };
        Ok(Paths {
            herds: config.join("herds"),
            home,
        })
    }
}

pub struct AddRequest {
    pub herd: String,
    pub template: String,
    pub params: Vec<String>,
    /// Passed to claude after the herd's and the template's args.
    pub claude_args: Vec<String>,
    pub ready_timeout: Duration,
}

/// Spawns a member of `req.herd`; returns its name with the outcome.
pub fn add(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    paths: &Paths,
    req: &AddRequest,
    inherited_workspace: Option<&str>,
) -> Result<(String, Spawned)> {
    let spawn_req = resolve(paths, req)?;
    let spawned = spawn(herdr, ledger, &spawn_req, inherited_workspace)?;
    Ok((spawn_req.name, spawned))
}

/// What `add` would do, one `key value` line each, then the prompt.
///
/// Makes only read-only herdr and git calls.
pub fn dry_run(
    herdr: &dyn Herdr,
    paths: &Paths,
    req: &AddRequest,
    inherited_workspace: Option<&str>,
) -> Result<String> {
    let spawn_req = resolve(paths, req)?;
    let plan = preview(herdr, &spawn_req, inherited_workspace)?;
    let worktree = plan.worktree.map(|p| p.display().to_string());
    let args: Vec<String> = spawn_req.claude_args.iter().map(|a| quote(a)).collect();
    Ok([
        format!("name {}", spawn_req.name),
        format!("branch {}", spawn_req.worktree.as_deref().unwrap_or("none")),
        format!("worktree {}", worktree.as_deref().unwrap_or("none")),
        format!("workspace {}", plan.workspace_label),
        format!("args {}", args.join(" ")),
        "prompt".into(),
        plan.prompt.unwrap_or_default(),
    ]
    .join("\n"))
}

fn quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(char::is_whitespace) {
        format!("{arg:?}")
    } else {
        arg.to_string()
    }
}

/// The spawn request for `req`, from the named herd file alone.
fn resolve(paths: &Paths, req: &AddRequest) -> Result<SpawnRequest> {
    let herd_file = paths.herds.join(format!("{}.toml", req.herd));
    let herd = load(&paths.herds, &req.herd).context("herd")?;
    let template = herd.template.get(&req.template).with_context(|| {
        let known: Vec<&str> = herd.template.keys().map(String::as_str).collect();
        format!(
            "template: {} has no template {} (it has: {})",
            req.herd,
            req.template,
            known.join(", ")
        )
    })?;
    if req.params.len() != template.params.len() {
        bail!(
            "template {} takes {} param{} ({}), got {}\n{USAGE}",
            req.template,
            template.params.len(),
            if template.params.len() == 1 { "" } else { "s" },
            template.params.join(", "),
            req.params.len()
        );
    }
    let params: Vec<(String, String)> = template
        .params
        .iter()
        .cloned()
        .zip(req.params.iter().cloned())
        .collect();
    let mut values: HashMap<&str, &str> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    values.insert("herd", &req.herd);

    let name = match &template.name {
        Some(pattern) => render(pattern, &values).context("name")?,
        None => [req.template.as_str()]
            .into_iter()
            .chain(req.params.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("-"),
    };
    values.insert("name", &name);
    let branch = match &template.branch {
        Some(pattern) => Some(render(pattern, &values).context("branch")?),
        None => None,
    };
    let prompt = match (&template.prompt, &template.prompt_file) {
        (Some(text), _) => text.clone(),
        (None, Some(file)) => {
            let base = herd_file.parent().unwrap_or(Path::new("."));
            let path = base.join(expand_home(file, &paths.home));
            std::fs::read_to_string(&path)
                .with_context(|| format!("prompt: read {}", path.display()))?
        }
        (None, None) => unreachable!("load rejects a template without a prompt"),
    };

    Ok(SpawnRequest {
        repo: expand_home(&herd.repo, &paths.home),
        name,
        worktree: branch,
        workspace: herd.workspace.clone(),
        prompt: Some(prompt),
        ready_timeout: req.ready_timeout,
        claude_args: [&herd.args[..], &template.args, &req.claude_args].concat(),
        member_of: Some(MemberOf {
            herd: req.herd.clone(),
            template: req.template.clone(),
            params,
        }),
    })
}

const USAGE: &str = "usage: loopherd add <herd> <template> <param>…";

fn render(pattern: &str, values: &HashMap<&str, &str>) -> Result<String> {
    Template::parse(pattern)?.render(values)
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// Every herd file in `paths.herds`, sorted by herd name, with a usage line
/// per template. Exits 1 if any file is invalid.
pub fn herds(paths: &Paths) -> Result<(String, i32)> {
    let files = herd_files(&paths.herds)?;
    if files.is_empty() {
        let hint = format!(
            "no herds: add a herd file at {}/<herd>.toml",
            paths.herds.display()
        );
        return Ok((hint, 0));
    }
    let mut lines = Vec::new();
    let mut code = 0;
    for file in &files {
        let herd = file.file_stem().unwrap_or_default().to_string_lossy();
        match std::fs::read_to_string(file)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse(&text))
        {
            Ok(parsed) => lines.extend(describe(&herd, &parsed)),
            Err(e) => {
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                lines.push(format!("invalid {name}: {}", format!("{e:#}").trim_end()));
                code = 1;
            }
        }
    }
    Ok((lines.join("\n"), code))
}

/// The `*.toml` files in `dir` by herd name, skipping hidden ones; none if `dir` is missing.
fn herd_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        entries => entries.with_context(|| format!("read {}", dir.display()))?,
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", dir.display()))?
            .path();
        let hidden = path
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."));
        if !hidden && path.extension().is_some_and(|e| e == "toml") {
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_stem().cmp(&b.file_stem()));
    Ok(files)
}

fn describe(herd: &str, parsed: &HerdFile) -> Vec<String> {
    let workspace = parsed.workspace.as_deref().unwrap_or("(current)");
    let header = format!("{herd}  repo {}  workspace {workspace}", parsed.repo);
    let usages = parsed.template.iter().map(|(template, t)| {
        let params: String = t.params.iter().map(|p| format!(" <{p}>")).collect();
        format!("  loopherd add {herd} {template}{params}")
    });
    std::iter::once(header).chain(usages).collect()
}

/// Placeholders every template gets, so no param may take their names.
const BUILT_INS: [&str; 5] = ["name", "herd", "branch", "worktree", "pane"];

fn load(dir: &Path, herd: &str) -> Result<HerdFile> {
    if herd.is_empty() || herd.starts_with('.') || herd.contains('/') {
        bail!("{herd:?} is not a herd name");
    }
    let file = dir.join(format!("{herd}.toml"));
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    parse(&text).with_context(|| file.display().to_string())
}

fn parse(text: &str) -> Result<HerdFile> {
    let parsed: HerdFile = toml::from_str(text)?;
    for (name, template) in &parsed.template {
        validate(template).with_context(|| format!("template {name}"))?;
    }
    Ok(parsed)
}

fn validate(template: &TemplateFile) -> Result<()> {
    match (&template.prompt, &template.prompt_file) {
        (Some(_), Some(_)) => bail!("set one of prompt and prompt_file, not both"),
        (None, None) => bail!("set one of prompt and prompt_file"),
        _ => {}
    }
    for (i, param) in template.params.iter().enumerate() {
        if BUILT_INS.contains(&param.as_str()) {
            bail!("param {param} is a built-in placeholder");
        }
        if template.params[..i].contains(param) {
            bail!("param {param} appears twice");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;

    const PR_HERD: &str = r#"
repo = "/src/repo"
args = ["--permission-mode", "auto"]

[template.pr]
params = ["number"]
name = "zpr-{number}"
prompt = "You're {name} in {herd}. Run `gh pr checkout {number}`."
args = ["--model", "opus"]
"#;

    struct Config {
        dir: tempfile::TempDir,
    }

    impl Config {
        fn with(herds: &[(&str, &str)]) -> Config {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("herds")).unwrap();
            for (name, text) in herds {
                fs::write(dir.path().join(format!("herds/{name}.toml")), text).unwrap();
            }
            Config { dir }
        }

        fn paths(&self) -> Paths {
            Paths {
                herds: self.dir.path().join("herds"),
                home: PathBuf::from("/home/me"),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn request(herd: &str, template: &str, params: &[&str]) -> AddRequest {
        AddRequest {
            herd: herd.into(),
            template: template.into(),
            params: params.iter().map(|p| p.to_string()).collect(),
            claude_args: vec![],
            ready_timeout: Duration::from_secs(60),
        }
    }

    fn herdr() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "zagops")])
    }

    #[test]
    fn adds_a_member_from_a_template_and_records_its_herd() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let herdr = herdr();
        let mut ledger = MemLedger::default();

        let (name, spawned) = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1050"]),
            Some("w1"),
        )
        .unwrap();

        assert_eq!(name, "zpr-1050");
        assert_eq!(spawned.report(&name).1, 0);
        assert_eq!(herdr.tabs()[0].label, "zpr-1050");
        assert_eq!(herdr.tabs()[0].cwd, PathBuf::from("/src/repo"));
        assert_eq!(
            herdr.prompts(),
            vec![(
                "zpr-1050".into(),
                "You're zpr-1050 in prs. Run `gh pr checkout 1050`.".into()
            )]
        );
        let rec = &ledger.0["zpr-1050"];
        assert_eq!(rec.herd.as_deref(), Some("prs"));
        assert_eq!(rec.template.as_deref(), Some("pr"));
    }

    fn add_err(config: &Config, req: &AddRequest) -> String {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let err = add(&herdr, &mut ledger, &config.paths(), req, Some("w1")).unwrap_err();
        assert!(herdr.tabs().is_empty(), "{err:#}");
        assert!(ledger.0.is_empty(), "{err:#}");
        format!("{err:#}")
    }

    #[test]
    fn a_missing_herd_file_is_an_error_naming_its_path() {
        let config = Config::with(&[]);
        let err = add_err(&config, &request("nope", "pr", &["1"]));
        let path = config.path().join("herds/nope.toml");
        assert!(
            err.starts_with(&format!("herd: read {}: ", path.display())),
            "{err}"
        );
    }

    #[test]
    fn a_herd_name_cannot_reach_outside_the_herds_directory() {
        let config = Config::with(&[]);
        fs::write(config.path().join("evil.toml"), PR_HERD).unwrap();
        for herd in ["../evil", "", "a/b"] {
            let err = add_err(&config, &request(herd, "pr", &["1"]));
            assert!(err.starts_with("herd: "), "{herd:?}: {err}");
            assert!(err.contains("not a herd name"), "{herd:?}: {err}");
        }
    }

    #[test]
    fn an_invalid_herd_file_is_an_error() {
        let template = "[template.pr]\nparams = []\nprompt = \"go\"\n";
        for (text, expected) in [
            ("repo = ".to_string(), "TOML parse error"),
            (
                format!("repo = \"/r\"\ncolour = 1\n{template}"),
                "unknown field `colour`",
            ),
            (
                format!("repo = \"/r\"\n{template}model = \"x\"\n"),
                "unknown field `model`",
            ),
            (template.to_string(), "missing field `repo`"),
            (
                "repo = \"/r\"\n[template.pr]\nprompt = \"go\"\n".to_string(),
                "missing field `params`",
            ),
            (
                format!("repo = \"/r\"\n{template}prompt_file = \"p.md\"\n"),
                "template pr: set one of prompt and prompt_file, not both",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = []\n".to_string(),
                "template pr: set one of prompt and prompt_file",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = [\"n\", \"n\"]\nprompt = \"go\"\n"
                    .to_string(),
                "template pr: param n appears twice",
            ),
            (
                "repo = \"/r\"\n[template.pr]\nparams = [\"name\"]\nprompt = \"go\"\n".to_string(),
                "template pr: param name is a built-in placeholder",
            ),
        ] {
            let config = Config::with(&[("h", &text)]);
            let err = add_err(&config, &request("h", "pr", &[]));
            assert!(err.starts_with("herd: "), "{text:?}: {err}");
            assert!(err.contains(expected), "{text:?}: {err}");
        }
    }

    #[test]
    fn an_unknown_template_is_an_error_listing_the_known_ones() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let err = add_err(&config, &request("prs", "issue", &["1"]));
        assert_eq!(err, "template: prs has no template issue (it has: pr)");
    }

    #[test]
    fn a_wrong_param_count_fails_with_usage() {
        let config = Config::with(&[("prs", PR_HERD)]);
        for params in [&[][..], &["1", "2"]] {
            let err = add_err(&config, &request("prs", "pr", params));
            assert_eq!(
                err,
                format!(
                    "template pr takes 1 param (number), got {}\n\
                     usage: loopherd add <herd> <template> <param>…",
                    params.len()
                )
            );
        }
    }

    fn add_ok(config: &Config, req: &AddRequest) -> (FakeHerdr, MemLedger, String) {
        let herdr = herdr();
        let mut ledger = MemLedger::default();
        let (name, _) = add(&herdr, &mut ledger, &config.paths(), req, Some("w1")).unwrap();
        (herdr, ledger, name)
    }

    #[test]
    fn the_default_name_is_the_template_then_the_params() {
        let herd = "repo = \"/r\"\n[template.pair]\nparams = [\"a\", \"b\"]\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        let (_, _, name) = add_ok(&config, &request("h", "pair", &["x", "y"]));
        assert_eq!(name, "pair-x-y");
    }

    #[test]
    fn the_branch_renders_params_herd_and_name_into_a_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = format!(
            "repo = {:?}\n[template.pr]\nparams = [\"n\"]\nname = \"z-{{n}}\"\n\
             branch = \"{{herd}}/{{name}}/{{n}}\"\nprompt = \"on {{branch}} in {{worktree}}\"\n",
            repo.display().to_string()
        );
        let config = Config::with(&[("prs", &herd)]);
        let (herdr, ledger, _) = add_ok(&config, &request("prs", "pr", &["7"]));
        let wt = root.path().join("proj/worktrees/prs-z-7-7");
        assert_eq!(herdr.tabs()[0].cwd, wt);
        assert_eq!(
            herdr.prompts()[0].1,
            format!("on prs/z-7/7 in {}", wt.display())
        );
        assert_eq!(ledger.0["z-7"].worktree, Some(wt));
    }

    #[test]
    fn unknown_placeholders_fail_before_anything_is_created() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        for (name, branch, prompt, message) in [
            ("z-{nmber}", "b", "go", "name: unknown placeholder {nmber}"),
            ("z-{pane}", "b", "go", "name: unknown placeholder {pane}"),
            (
                "z",
                "b-{worktree}",
                "go",
                "branch: unknown placeholder {worktree}",
            ),
            (
                "z",
                "b",
                "hi {nmber}",
                "prompt: unknown placeholder {nmber}",
            ),
        ] {
            let herd = format!(
                "repo = {:?}\n[template.pr]\nparams = [\"number\"]\n\
                 name = {name:?}\nbranch = {branch:?}\nprompt = {prompt:?}\n",
                repo.display().to_string()
            );
            let config = Config::with(&[("prs", &herd)]);
            let err = add_err(&config, &request("prs", "pr", &["1"]));
            assert_eq!(err, message, "{herd}");
            assert!(!root.path().join("proj/worktrees").exists());
        }
    }

    #[test]
    fn a_leading_tilde_in_repo_is_the_home_directory() {
        let herd = "repo = \"~/src/repo\"\n[template.t]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("h", herd)]);
        let (herdr, ledger, _) = add_ok(&config, &request("h", "t", &[]));
        assert_eq!(herdr.tabs()[0].cwd, PathBuf::from("/home/me/src/repo"));
        assert_eq!(ledger.0["t"].repo, PathBuf::from("/home/me/src/repo"));
    }

    #[test]
    fn prompt_file_is_read_relative_to_the_herd_file_and_rendered() {
        let herd = "repo = \"/r\"\n[template.t]\nparams = [\"n\"]\nprompt_file = \"t.md\"\n";
        let config = Config::with(&[("h", herd)]);
        fs::write(config.path().join("herds/t.md"), "{name} takes {n}\n").unwrap();
        let (herdr, _, _) = add_ok(&config, &request("h", "t", &["5"]));
        assert_eq!(herdr.prompts()[0].1, "t-5 takes 5\n");
    }

    #[test]
    fn a_missing_prompt_file_fails_before_anything_is_created() {
        let herd = "repo = \"/r\"\n[template.t]\nparams = []\nprompt_file = \"t.md\"\n";
        let config = Config::with(&[("h", herd)]);
        let err = add_err(&config, &request("h", "t", &[]));
        let path = config.path().join("herds/t.md");
        assert!(
            err.starts_with(&format!("prompt: read {}: ", path.display())),
            "{err}"
        );
    }

    #[test]
    fn claude_args_are_the_herds_then_the_templates_then_the_command_lines() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let req = AddRequest {
            claude_args: vec!["--verbose".into()],
            ..request("prs", "pr", &["1"])
        };
        let (herdr, ledger, _) = add_ok(&config, &req);
        let expected = ["--permission-mode", "auto", "--model", "opus", "--verbose"];
        assert_eq!(herdr.started()[0].args, expected);
        assert_eq!(ledger.0["zpr-1"].claude_args, expected);
    }

    #[test]
    fn the_workspace_is_the_herds_label_or_else_the_current_one() {
        let labeled = PR_HERD.replace(
            "args = [\"--permission",
            "workspace = \"zagops\"\nargs = [\"--permission",
        );
        let config = Config::with(&[("prs", PR_HERD), ("zag", &labeled)]);
        let (herdr, _, _) = add_ok(&config, &request("prs", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w1");
        let (herdr, ledger, _) = add_ok(&config, &request("zag", "pr", &["1"]));
        assert_eq!(herdr.tabs()[0].workspace_id, "w2");
        assert_eq!(ledger.0["zpr-1"].workspace_label.as_deref(), Some("zagops"));
    }

    #[test]
    fn a_workspace_label_matching_no_workspace_fails_before_anything_is_created() {
        let herd = PR_HERD.replace(
            "args = [\"--permission",
            "workspace = \"nope\"\nargs = [\"--permission",
        );
        let config = Config::with(&[("prs", &herd)]);
        let err = add_err(&config, &request("prs", "pr", &["1"]));
        assert_eq!(err, "workspace: no workspace labeled \"nope\"");
    }

    #[test]
    fn spawns_checks_still_apply_to_members() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let herdr = herdr().with_agent("zpr-1");
        let mut ledger = MemLedger::default();
        let err = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("already a live agent"),
            "{err:#}"
        );
        assert!(herdr.tabs().is_empty());

        let err = add_err(&config, &request("prs", "pr", &["Bad/Name"]));
        assert!(err.starts_with("name: "), "{err}");
    }

    #[test]
    fn a_member_blocked_at_startup_exits_3_and_a_failed_start_closes_its_tab() {
        use crate::herdr::fake::Startup;
        let config = Config::with(&[("prs", PR_HERD)]);
        let mut herdr = herdr();
        herdr.startup = Startup::BlockedForever;
        let mut ledger = MemLedger::default();
        let (name, spawned) = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap();
        assert_eq!(spawned.report(&name).1, 3);
        assert!(!ledger.0["zpr-1"].prompt_sent);

        let mut herdr = self::herdr();
        herdr.startup = Startup::Fails;
        let mut ledger = MemLedger::default();
        let err = add(
            &herdr,
            &mut ledger,
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").starts_with("start: "), "{err:#}");
        assert!(herdr.tabs().is_empty());
        assert!(ledger.0.is_empty());
    }

    #[test]
    fn dry_run_prints_the_resolved_member_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = crate::worktree::testing::repo_with_origin(root.path());
        let herd = format!(
            "repo = {:?}\nworkspace = \"zagops\"\nargs = [\"--model\", \"opus\"]\n\
             [template.pr]\nparams = [\"n\"]\nname = \"z-{{n}}\"\nbranch = \"pr-{{n}}\"\n\
             prompt = \"{{name}} on {{branch}} in {{worktree}}, pane {{pane}}\"\n",
            repo.display().to_string()
        );
        let config = Config::with(&[("prs", &herd)]);
        let herdr = herdr();
        let req = AddRequest {
            claude_args: vec!["--add-dir".into(), "/tmp/a b".into()],
            ..request("prs", "pr", &["7"])
        };

        let report = dry_run(&herdr, &config.paths(), &req, Some("w1")).unwrap();

        let wt = root.path().join("proj/worktrees/pr-7");
        assert_eq!(
            report,
            format!(
                "name z-7\nbranch pr-7\nworktree {wt}\nworkspace zagops\n\
                 args --model opus --add-dir \"/tmp/a b\"\nprompt\n\
                 z-7 on pr-7 in {wt}, pane {{pane}}",
                wt = wt.display()
            )
        );
        assert!(herdr.tabs().is_empty());
        assert!(herdr.started().is_empty());
        assert!(herdr.prompts().is_empty());
        assert!(!root.path().join("proj/worktrees").exists());
    }

    #[test]
    fn dry_run_without_a_branch_runs_in_the_repo_and_still_checks_the_workspace() {
        let config = Config::with(&[("prs", PR_HERD)]);
        let report = dry_run(
            &herdr(),
            &config.paths(),
            &request("prs", "pr", &["1"]),
            Some("w1"),
        )
        .unwrap();
        assert!(
            report.starts_with("name zpr-1\nbranch none\nworktree none\nworkspace loopherd\n"),
            "{report}"
        );
        let err = dry_run(
            &herdr(),
            &config.paths(),
            &request("prs", "pr", &["1"]),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").starts_with("workspace: "), "{err:#}");
    }

    #[test]
    fn herds_lists_each_herd_with_a_usage_line_per_template_sorted_by_name() {
        let issues = "repo = \"~/src/z\"\nworkspace = \"zagops\"\n\
                      [template.issue]\nparams = [\"key\", \"slug\"]\nprompt = \"go\"\n\
                      [template.chat]\nparams = []\nprompt = \"go\"\n";
        let config = Config::with(&[("zissues", issues), ("prs", PR_HERD)]);

        let (report, code) = herds(&config.paths()).unwrap();

        assert_eq!(
            report,
            "prs  repo /src/repo  workspace (current)\n\
             \x20 loopherd add prs pr <number>\n\
             zissues  repo ~/src/z  workspace zagops\n\
             \x20 loopherd add zissues chat\n\
             \x20 loopherd add zissues issue <key> <slug>"
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn herds_sorts_by_herd_name_not_file_name() {
        let config = Config::with(&[("prs-old", PR_HERD), ("prs", PR_HERD)]);
        let (report, _) = herds(&config.paths()).unwrap();
        let headers: Vec<&str> = report.lines().filter(|l| !l.starts_with(' ')).collect();
        assert_eq!(headers[0], "prs  repo /src/repo  workspace (current)");
        assert!(headers[1].starts_with("prs-old  "), "{report}");
    }

    #[test]
    fn herds_reports_invalid_files_lists_the_rest_and_exits_1() {
        let config = Config::with(&[
            ("a-broken", "repo = "),
            ("prs", PR_HERD),
            ("z-bad", "repo = \"/r\"\n[template.t]\nparams = []\n"),
        ]);

        let (report, code) = herds(&config.paths()).unwrap();

        let lines: Vec<&str> = report.lines().collect();
        assert!(
            lines[0].starts_with("invalid a-broken.toml: TOML parse error"),
            "{report}"
        );
        assert!(!lines[0].contains(&config.path().display().to_string()));
        assert_eq!(
            lines[lines.len() - 3..],
            [
                "prs  repo /src/repo  workspace (current)",
                "  loopherd add prs pr <number>",
                "invalid z-bad.toml: template t: set one of prompt and prompt_file",
            ]
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn herds_with_no_herd_files_hints_at_the_directory_and_exits_0() {
        let config = Config::with(&[]);
        fs::write(config.path().join("herds/notes.md"), "").unwrap();
        fs::write(config.path().join("herds/.draft.toml"), "repo = ").unwrap();
        let missing = Paths {
            herds: config.path().join("nope/herds"),
            ..config.paths()
        };
        for paths in [config.paths(), missing] {
            let (report, code) = herds(&paths).unwrap();
            assert_eq!(
                report,
                format!(
                    "no herds: add a herd file at {}/<herd>.toml",
                    paths.herds.display()
                )
            );
            assert_eq!(code, 0);
        }
    }
}
