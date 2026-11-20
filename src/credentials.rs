//! The split credentials of a lead guarded by `merge_guard = "credentials"`:
//! a deploy key for its git and a token for its gh that can't merge (D83, D95).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::herd::Paths;

/// Where a herd's lead's credentials are kept, in loopherd's config dir.
pub struct Files {
    pub key: PathBuf,
    pub token: PathBuf,
}

pub fn files(paths: &Paths, herd: &str) -> Files {
    let dir = paths.config_dir().join("credentials");
    Files {
        key: dir.join(format!("{herd}.key")),
        token: dir.join(format!("{herd}.token")),
    }
}

/// The environment `herd`'s lead runs in: its token for gh, no ssh agent or
/// credential helper, and git fetching from and pushing to the herd's GitHub
/// repo through its `Host` alias. Errs, naming
/// the fix, while the key or the token is missing.
pub fn lead_env(paths: &Paths, herd: &str) -> Result<Map<String, Value>> {
    if let Some(missing) = missing(paths, herd) {
        bail!("{missing}");
    }
    let token = std::fs::read_to_string(files(paths, herd).token)?;
    let origin = crate::github::github_com_origin(&crate::herd::repo(paths, herd)?)?;
    let slug = &origin.slug;
    let mut urls = vec![origin.url.clone()];
    for url in [
        format!("https://github.com/{slug}.git"),
        format!("git@github.com:{slug}.git"),
    ] {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    let alias = format!("url.git@{}:{slug}.git", host_alias(herd));
    // An empty helper clears the human's; pushInsteadOf beats their own for pushes.
    let mut config = vec![("credential.helper".to_string(), String::new())];
    for url in urls {
        config.push((format!("{alias}.insteadOf"), url.clone()));
        config.push((format!("{alias}.pushInsteadOf"), url));
    }
    let mut env = vec![
        ("GH_TOKEN".to_string(), token.trim().to_string()),
        ("SSH_AUTH_SOCK".to_string(), String::new()),
        ("GIT_CONFIG_COUNT".to_string(), config.len().to_string()),
    ];
    for (i, (key, value)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), value));
    }
    Ok(env
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect())
}

/// What a credentials lead lacks, and how to fix it.
pub struct Missing {
    pub what: String,
    pub fix: String,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}; {}", self.what, self.fix)
    }
}

/// What `herd`'s lead lacks of its deploy key and token.
pub fn missing(paths: &Paths, herd: &str) -> Option<Missing> {
    let files = files(paths, herd);
    if !files.key.is_file() {
        return Some(Missing {
            what: format!("no deploy key at {}", files.key.display()),
            fix: format!("run loopherd merge-guard {herd}"),
        });
    }
    let token = std::fs::read_to_string(&files.token).unwrap_or_default();
    token.trim().is_empty().then(|| Missing {
        what: format!("no token at {}", files.token.display()),
        fix: format!("run loopherd merge-guard {herd} for how to create and store it"),
    })
}

/// The ssh `Host` that reaches GitHub with `herd`'s deploy key alone.
fn host_alias(herd: &str) -> String {
    format!("loopherd-{herd}")
}

/// loopherd's own ssh config, holding each credentials herd's `Host` block,
/// which the human's `~/.ssh/config` includes.
pub fn ssh_config(paths: &Paths) -> PathBuf {
    paths.config_dir().join("ssh/config")
}

/// The line the human adds at the top of `~/.ssh/config`.
pub fn include_line(paths: &Paths) -> String {
    let config = ssh_config(paths);
    let shown = match config.strip_prefix(&paths.home) {
        Ok(under) => format!("~/{}", under.display()),
        Err(_) => config.display().to_string(),
    };
    format!("Include {shown}")
}

fn host_block(paths: &Paths, herd: &str) -> String {
    format!(
        "Host {}\n  HostName github.com\n  User git\n  IdentityFile \"{}\"\n  \
         IdentitiesOnly yes\n  IdentityAgent none\n",
        host_alias(herd),
        files(paths, herd).key.display()
    )
}

/// An ssh config line's keyword, lowercased as ssh matches it, and its
/// arguments, unquoted.
fn keyword(line: &str) -> Option<(String, Vec<&str>)> {
    let mut words = line
        .split(|c: char| c.is_whitespace() || c == '=')
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('"'));
    let keyword = words.next().filter(|k| !k.starts_with('#'))?;
    Some((keyword.to_ascii_lowercase(), words.collect()))
}

fn starts_block(line: &str) -> bool {
    keyword(line).is_some_and(|(k, _)| k == "host" || k == "match")
}

/// loopherd's ssh config split into its `Host` and `Match` blocks.
fn host_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = vec![];
    for line in text.lines() {
        match blocks.last_mut() {
            Some(block) if !starts_block(line) => block.push_str(line),
            _ => blocks.push(line.to_string()),
        }
        blocks.last_mut().unwrap().push('\n');
    }
    blocks
}

fn is_block_of(block: &str, alias: &str) -> bool {
    block
        .lines()
        .next()
        .and_then(keyword)
        .is_some_and(|(k, args)| k == "host" && args.contains(&alias))
}

/// Writes `herd`'s `Host` block into loopherd's ssh config, replacing an old
/// one. Returns whether the file changed.
pub fn write_host_block(paths: &Paths, herd: &str) -> Result<bool> {
    let file = ssh_config(paths);
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let ours = host_block(paths, herd);
    let alias = host_alias(herd);
    let mut blocks = host_blocks(&text);
    match blocks.iter_mut().find(|b| is_block_of(b, &alias)) {
        Some(block) if *block == ours => return Ok(false),
        Some(block) => *block = ours,
        None => blocks.push(ours),
    }
    let dir = file.parent().context("ssh config dir")?;
    std::fs::create_dir_all(dir)?;
    // ssh refuses an included file others can write, which would break all the human's ssh.
    let temp = dir.join(".config.new");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(blocks.concat().as_bytes())?;
    std::fs::rename(&temp, &file)?;
    Ok(true)
}

/// What keeps `herd`'s lead's git from reaching GitHub through its alias:
/// the human's `~/.ssh/config` not including loopherd's, before any `Host`
/// or `Match`, or loopherd's lacking the herd's current block.
pub fn ssh_missing(paths: &Paths, herd: &str) -> Option<Missing> {
    let human = paths.home.join(".ssh/config");
    let include = include_line(paths);
    let config = ssh_config(paths);
    let wanted = [
        include["Include ".len()..].to_string(),
        config.display().to_string(),
    ];
    let included = std::fs::read_to_string(&human)
        .unwrap_or_default()
        .lines()
        .take_while(|line| !starts_block(line))
        .filter_map(keyword)
        .any(|(k, args)| k == "include" && args.iter().any(|a| wanted.iter().any(|w| w == a)));
    if !included {
        return Some(Missing {
            what: format!(
                "{} doesn't include loopherd's ssh config before its first Host",
                human.display()
            ),
            fix: format!("add {include} as its first line"),
        });
    }
    let text = std::fs::read_to_string(ssh_config(paths)).unwrap_or_default();
    let ours = host_block(paths, herd);
    (!host_blocks(&text).contains(&ours)).then(|| Missing {
        what: format!(
            "no current Host {} in {}",
            host_alias(herd),
            ssh_config(paths).display()
        ),
        fix: format!("run loopherd merge-guard {herd}"),
    })
}

/// Where gh, git and ssh keep the human's GitHub credentials, each with
/// whether it's a directory.
pub fn human_credentials(home: &Path) -> [(PathBuf, bool); 6] {
    [
        (home.join(".ssh"), true),
        (home.join(".netrc"), false),
        (home.join(".config/gh"), true),
        (home.join(".git-credentials"), false),
        (home.join(".config/git/credentials"), false),
        (home.join(".cache/git/credential"), true),
    ]
}

/// What ssh still reads under a denied `~/.ssh` to reach a `Host` alias.
pub fn ssh_readable(home: &Path) -> [PathBuf; 2] {
    [home.join(".ssh/config"), home.join(".ssh/known_hosts")]
}
