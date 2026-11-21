//! `loopherd merge-guard`: sets up what a lead guarded by `merge_guard =
//! "credentials"` needs on GitHub, with the human's own gh login (D95).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::credentials;
use crate::github::all_pages;
use crate::herd::{self, MergeGuard, Paths};

pub const DEFAULT_BRANCH_RULESET: &str = "loopherd: default branch";
pub const FORCE_PUSH_RULESET: &str = "loopherd: no force-push";

/// Makes `herd`'s lead's deploy key if missing, adds it to the herd's GitHub
/// repo unless it's there, creates or updates both rulesets, and returns what
/// it did followed by how to create and store the token.
pub fn setup(paths: &Paths, herd: &str) -> Result<Vec<String>> {
    match herd::merge_guard(paths, herd)? {
        Some(MergeGuard::Credentials) => {}
        Some(MergeGuard::Hook) => bail!(
            "herd {herd}'s lead has merge_guard = \"hook\"; run loopherd herd set {herd} \
             lead.merge_guard '\"credentials\"' first"
        ),
        None => {
            herd::lead_switches(paths, herd)?;
            bail!("herd {herd}'s lead has merge on, so merge_guard doesn't apply")
        }
    }
    let rebase = herd::lead_switches(paths, herd)?.rebase;
    let slug = crate::github::github_com_origin(&herd::repo(paths, herd)?)?.slug;
    let files = credentials::files(paths, herd);
    let mut lines = vec![];

    if !files.key.exists() {
        make_key(&files.key, herd, &slug)?;
        lines.push(format!("made the deploy key {}", files.key.display()));
    }
    if credentials::write_host_block(paths, herd)? {
        lines.push(format!(
            "wrote Host loopherd-{herd} into {}",
            credentials::ssh_config(paths).display()
        ));
    }
    let public = format!("{}.pub", files.key.display());
    let public_text = match std::fs::read_to_string(&public) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "the deploy key {} has no {public}; make it with ssh-keygen -y -f {} > {public}",
            files.key.display(),
            files.key.display()
        ),
        read => read.with_context(|| format!("read {public}"))?,
    };
    let added: Vec<DeployKey> = all_pages(&gh(
        &[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{slug}/keys"),
        ],
        None,
    )?)
    .context("read gh's list of deploy keys")?;
    if let Some(key) = added.iter().find(|k| same_key(&k.key, &public_text)) {
        if key.read_only {
            bail!(
                "{slug} has the deploy key read-only, so the lead can't push; delete it with \
                 gh repo deploy-key delete {} --repo {slug}, then run loopherd merge-guard {herd} \
                 again",
                key.id
            );
        }
        lines.push(format!("{slug} already has the deploy key"));
    } else {
        let title = format!("loopherd lead-{herd}");
        let add = [
            "repo",
            "deploy-key",
            "add",
            &public,
            "--repo",
            &slug,
            "--allow-write",
            "--title",
            &title,
        ];
        gh(&add, None)?;
        lines.push(format!("added it to {slug} as a write deploy key"));
    }

    let listed: Vec<Ruleset> = all_pages(&gh(
        &[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{slug}/rulesets?includes_parents=false"),
        ],
        None,
    )?)
    .context("read gh's list of rulesets")?;
    for (name, body) in [
        (DEFAULT_BRANCH_RULESET, default_branch_ruleset()),
        (FORCE_PUSH_RULESET, force_push_ruleset(rebase)),
    ] {
        let body = serde_json::to_string_pretty(&body)? + "\n";
        let (method, path, done) = match listed.iter().find(|r| r.name == name) {
            Some(r) => ("PUT", format!("repos/{slug}/rulesets/{}", r.id), "updated"),
            None => ("POST", format!("repos/{slug}/rulesets"), "created"),
        };
        gh(
            &["api", "--method", method, &path, "--input", "-"],
            Some(&body),
        )?;
        lines.push(format!("{done} ruleset {name}"));
    }
    if credentials::ssh_missing(paths, herd).is_some() {
        lines.push(include_step(paths));
    }
    lines.push(token_steps(&slug, herd, &files.token));
    Ok(lines)
}

/// Updates and deletions of the default branch only through a pull request
/// merged by a repository admin.
pub fn default_branch_ruleset() -> Value {
    json!({
        "name": DEFAULT_BRANCH_RULESET,
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "update"}, {"type": "deletion"}],
        "bypass_actors": [
            {"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "pull_request"}
        ],
    })
}

/// No force-push to any branch, except by deploy keys while the lead may rebase.
pub fn force_push_ruleset(rebase: bool) -> Value {
    let bypass = match rebase {
        true => json!([{"actor_id": null, "actor_type": "DeployKey", "bypass_mode": "always"}]),
        false => json!([]),
    };
    json!({
        "name": FORCE_PUSH_RULESET,
        "target": "branch",
        "enforcement": "active",
        "conditions": {"ref_name": {"include": ["~ALL"], "exclude": []}},
        "rules": [{"type": "non_fast_forward"}],
        "bypass_actors": bypass,
    })
}

#[derive(Deserialize)]
struct DeployKey {
    id: u64,
    key: String,
    read_only: bool,
}

#[derive(Deserialize)]
struct Ruleset {
    id: u64,
    name: String,
}

/// GitHub lists a key as its type and body, without the comment.
fn same_key(listed: &str, public: &str) -> bool {
    let public: Vec<&str> = public.split_whitespace().take(2).collect();
    public.len() == 2 && listed.split_whitespace().take(2).eq(public)
}

fn make_key(key: &Path, herd: &str, slug: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = key.parent().context("credentials dir")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let comment = format!("loopherd lead-{herd} {slug}");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
        .arg(key)
        .stdin(Stdio::null())
        .output()
        .context("run ssh-keygen")?;
    if !made.status.success() {
        bail!(
            "ssh-keygen: {}",
            String::from_utf8_lossy(&made.stderr).trim()
        );
    }
    Ok(())
}

/// gh's stdout, as the human's own gh login, with `input` on its stdin.
fn gh(args: &[&str], input: Option<&str>) -> Result<String> {
    let mut child = Command::new("gh")
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run gh; is the GitHub CLI installed?")?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes()).context("write to gh")?;
    }
    let out = child.wait_with_output().context("wait for gh")?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn include_step(paths: &Paths) -> String {
    format!(
        "\nThe lead's git reaches GitHub as Host loopherd-<herd>, from loopherd's own ssh config. \
         Add this as the first line of ~/.ssh/config, once for every herd (loopherd never \
         writes there):\n   {}",
        credentials::include_line(paths)
    )
}

fn token_steps(slug: &str, herd: &str, token: &Path) -> String {
    let owner = slug.split('/').next().unwrap_or(slug);
    format!(
        "\nLast, the lead's token, which GitHub makes only in the web UI:\n\
         1. Open https://github.com/settings/personal-access-tokens/new\n\
         2. Name it loopherd lead-{herd}, resource owner {owner}, expiration No expiration.\n\
         3. Repository access: Only select repositories, {slug}.\n\
         4. Permissions: Pull requests: Read and write, Contents: Read-only, and for a herd \
         whose tasks are GitHub issues, Issues: Read and write (Metadata: Read-only comes \
         with them). Nothing else.\n\
         5. Generate it, then store it where only you can read it: run\n   \
         (umask 077 && cat > {token})\n   \
         paste the token, press Enter and Ctrl-D.\n\
         Then start the lead: loopherd lead {herd}.\n\n\
         The default-branch ruleset stops plain merges, yours too: merge with \
         gh pr merge --admin, or \"Merge without waiting for requirements\" in the web UI. \
         On a private repo the token can't read check results, so the lead reads them \
         through gh pr view --json or the Actions API.",
        token = token.display()
    )
}
