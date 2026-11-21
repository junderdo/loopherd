//! The split credentials of a lead guarded by `merge_guard = "credentials"`:
//! a deploy key for its git and a token for its gh that can't merge (D83, D95).

use std::ffi::OsString;
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
        ("GITHUB_TOKEN".to_string(), String::new()),
        ("GH_ENTERPRISE_TOKEN".to_string(), String::new()),
        ("GITHUB_ENTERPRISE_TOKEN".to_string(), String::new()),
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
    let dir = file.parent().context("ssh config dir")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let lock_file = dir.join(".lock");
    let lock = std::fs::File::create(&lock_file)
        .with_context(|| format!("open {}", lock_file.display()))?;
    let _held = crate::lock::Lock::wait(lock, crate::lock::BRIEF)
        .with_context(|| format!("lock {}", lock_file.display()))?;
    let text = match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.with_context(|| format!("read {}", file.display()))?,
    };
    let ours = host_block(paths, herd);
    let alias = host_alias(herd);
    let mut blocks = host_blocks(&text);
    match blocks.iter_mut().find(|b| is_block_of(b, &alias)) {
        Some(block) if *block == ours => return Ok(false),
        Some(block) => *block = ours,
        None => blocks.push(ours),
    }
    // ssh refuses an included file others can write, which would break all the human's ssh.
    let temp = dir.join(format!(".config.{}", std::process::id()));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(blocks.concat().as_bytes())?;
    std::fs::rename(&temp, &file).with_context(|| format!("write {}", file.display()))?;
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
    host_block_missing(paths, herd)
}

/// What keeps ssh, reading loopherd's ssh config, from reaching GitHub
/// through `herd`'s alias: the config lacking the herd's current block.
pub fn host_block_missing(paths: &Paths, herd: &str) -> Option<Missing> {
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
/// whether it's a directory: their default places, and those `env` moves
/// them to (`XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `GH_CONFIG_DIR`).
pub fn human_credentials(
    home: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(PathBuf, bool)> {
    let dir = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let config = dir("XDG_CONFIG_HOME").unwrap_or(home.join(".config"));
    let cache = dir("XDG_CACHE_HOME").unwrap_or(home.join(".cache"));
    let mut found = vec![
        (home.join(".ssh"), true),
        (home.join(".netrc"), false),
        (home.join(".config/gh"), true),
        (home.join(".git-credentials"), false),
        (home.join(".config/git/credentials"), false),
        (home.join(".cache/git/credential"), true),
    ];
    let moved = [
        (config.join("gh"), true),
        (config.join("git/credentials"), false),
        (cache.join("git/credential"), true),
    ];
    for path in moved
        .into_iter()
        .chain(dir("GH_CONFIG_DIR").map(|d| (d, true)))
    {
        if !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// What ssh still reads under a denied `~/.ssh` to reach a `Host` alias.
fn ssh_readable(home: &Path) -> [PathBuf; 2] {
    [home.join(".ssh/config"), home.join(".ssh/known_hosts")]
}

/// What a sandboxed member of credentials herd `herd` runs with, and what
/// of the human's GitHub credentials, other herds' and members' is kept
/// from it.
pub struct Confined {
    pub env: Map<String, Value>,
    /// Denied to the sandbox and the file tools alike, each marked `true` when a directory.
    pub deny_read: Vec<(PathBuf, bool)>,
    /// Opened back inside `deny_read`, to the sandbox alone.
    pub allow_read: Vec<PathBuf>,
}

/// [`Confined`] for a member of `herd`, the human's credentials found by
/// `env` and Windows drives by `mounts`, the text of `/proc/mounts`. Errs,
/// naming the fix, while the herd's key or token is missing.
pub fn confined(
    paths: &Paths,
    herd: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
    mounts: &str,
) -> Result<Confined> {
    let config = paths.config_dir();
    let mut deny_read = human_credentials(&paths.home, env);
    deny_read.push((config.to_path_buf(), true));
    deny_read.extend(drvfs_mounts(mounts).into_iter().map(|dir| (dir, true)));
    let mut allow_read = ssh_readable(&paths.home).to_vec();
    allow_read.extend(["herds", "config.toml", "trello.env"].map(|path| config.join(path)));
    allow_read.push(ssh_config(paths));
    allow_read.push(files(paths, herd).key);
    Ok(Confined {
        env: lead_env(paths, herd)?,
        deny_read,
        allow_read,
    })
}

/// Where `/proc/mounts` has a Windows drive mounted: WSL1 lists drvfs as
/// `drvfs`, WSL2 as `9p` with `aname=drvfs`, or with virtiofs on as
/// `virtiofs` from its `drvfs` or `drvfsa` tag.
fn drvfs_mounts(mounts: &str) -> Vec<PathBuf> {
    mounts
        .lines()
        .filter_map(|line| {
            let [source, dir, kind, options] =
                line.split_whitespace().take(4).collect::<Vec<_>>()[..]
            else {
                return None;
            };
            let drvfs = match kind {
                "drvfs" => true,
                "9p" => options.split([',', ';']).any(|o| o == "aname=drvfs"),
                "virtiofs" => matches!(source, "drvfs" | "drvfsa"),
                _ => false,
            };
            drvfs.then(|| PathBuf::from(unescape_mount(dir)))
        })
        .collect()
}

/// A `/proc/mounts` field with its octal escapes (`\040` for a space) undone.
fn unescape_mount(field: &str) -> String {
    let mut out = String::new();
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let code = rest
            .get(at + 1..at + 4)
            .and_then(|d| u8::from_str_radix(d, 8).ok());
        match code {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_humans_credentials_are_found_where_the_environment_moves_them() {
        let home = Path::new("/home/me");
        let env = |name: &str| match name {
            "XDG_CONFIG_HOME" => Some("/home/me/.xdg".into()),
            "XDG_CACHE_HOME" => Some("".into()),
            "GH_CONFIG_DIR" => Some("/elsewhere/gh".into()),
            _ => None,
        };

        let found = human_credentials(home, &env);

        assert_eq!(
            found[6..],
            [
                (PathBuf::from("/home/me/.xdg/gh"), true),
                (PathBuf::from("/home/me/.xdg/git/credentials"), false),
                (PathBuf::from("/elsewhere/gh"), true),
            ]
        );
        assert_eq!(human_credentials(home, &|_| None).len(), 6);
    }

    /// Herd `h` on a GitHub repo, with its deploy key and token, kept under `root`.
    fn credentials_herd(root: &Path) -> Paths {
        let paths = Paths {
            herds: root.join("config/herds"),
            home: PathBuf::from("/home/me"),
            feed_lock: root.join("feed.lock"),
            paused: root.join("paused"),
        };
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        crate::worktree::testing::git(&repo, &["init", "--quiet"]).unwrap();
        let url = "https://github.com/acme/widgets.git";
        crate::worktree::testing::git(&repo, &["remote", "add", "origin", url]).unwrap();
        std::fs::create_dir_all(&paths.herds).unwrap();
        std::fs::write(
            paths.herds.join("h.toml"),
            format!("repo = {:?}\n", repo.to_str().unwrap()),
        )
        .unwrap();
        let files = files(&paths, "h");
        std::fs::create_dir_all(files.key.parent().unwrap()).unwrap();
        std::fs::write(&files.key, "a private key\n").unwrap();
        std::fs::write(&files.token, "github_pat_1\n").unwrap();
        paths
    }

    #[test]
    fn every_windows_drive_is_denied_and_a_machine_without_one_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = credentials_herd(dir.path());
        let mounts = "\
/dev/sdc / ext4 rw,relatime 0 0
none /mnt/wsl tmpfs rw,relatime 0 0
C:\\134 /mnt/c 9p rw,noatime,aname=drvfs;path=C:\\;uid=1000;symlinkroot=/mnt/ 0 0
C:\\134Program\\040Files /Docker/host\\040files 9p rw,aname=drvfs;path=C:\\Program Files 0 0
D: /mnt/d drvfs rw,noatime 0 0
drvfs /run/wsl/virtiofs-mounts/drvfs virtiofs rw,relatime 0 0
drvfsa /mnt/e virtiofs rw,relatime 0 0
shared /mnt/shared virtiofs rw,relatime 0 0
server:/share /mnt/nfs 9p rw,aname=other 0 0
";
        let denied = |mounts: &str| {
            let confined = confined(&paths, "h", &|_| None, mounts).unwrap();
            confined.deny_read[7..].to_vec()
        };

        assert_eq!(
            denied(mounts),
            [
                (PathBuf::from("/mnt/c"), true),
                (PathBuf::from("/Docker/host files"), true),
                (PathBuf::from("/mnt/d"), true),
                (PathBuf::from("/run/wsl/virtiofs-mounts/drvfs"), true),
                (PathBuf::from("/mnt/e"), true),
            ]
        );
        assert_eq!(denied("/dev/sdc / ext4 rw,relatime 0 0\n"), []);
    }

    #[test]
    fn a_host_block_isnt_written_over_an_ssh_config_that_cant_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            herds: dir.path().join("config/herds"),
            home: dir.path().to_path_buf(),
            feed_lock: dir.path().join("feed.lock"),
            paused: dir.path().join("paused"),
        };
        let file = ssh_config(&paths);
        std::fs::create_dir_all(&file).unwrap();

        let err = write_host_block(&paths, "h").unwrap_err();

        assert_eq!(format!("{err}"), format!("read {}", file.display()));
        assert!(file.is_dir());
    }
}
