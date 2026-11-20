//! The split credentials of a lead guarded by `merge_guard = "credentials"`:
//! a deploy key for its git and a token for its gh that can't merge (D83, D95).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::herd::Paths;
use crate::shell::quote;

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

/// The environment `herd`'s lead runs in: its token for gh, and git pushing
/// to the herd's GitHub repo over ssh with the deploy key alone. Errs, naming
/// the fix, while the key or the token is missing.
pub fn lead_env(paths: &Paths, herd: &str) -> Result<Map<String, Value>> {
    if let Some(missing) = missing(paths, herd) {
        bail!("{missing}");
    }
    let files = files(paths, herd);
    let token = std::fs::read_to_string(&files.token)?;
    let token = token.trim();
    let origin = crate::github::github_com_origin(&crate::herd::repo(paths, herd)?)?;
    // ssh reaches GitHub only through the sandbox's proxy, and must offer no key of the human's.
    let ssh = format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes \
         -o ProxyCommand='loopherd ssh-proxy %h %p'",
        quote(&files.key.to_string_lossy())
    );
    let env = [
        ("GH_TOKEN", token.to_string()),
        ("GIT_CONFIG_COUNT", "2".into()),
        ("GIT_CONFIG_KEY_0", "core.sshCommand".into()),
        ("GIT_CONFIG_VALUE_0", ssh),
        (
            "GIT_CONFIG_KEY_1",
            format!("url.git@github.com:{}.git.insteadOf", origin.slug),
        ),
        ("GIT_CONFIG_VALUE_1", origin.url),
    ];
    Ok(env
        .into_iter()
        .map(|(name, value)| (name.to_string(), Value::String(value)))
        .collect())
}

/// What `herd`'s lead lacks of its deploy key and token, with the fix.
pub fn missing(paths: &Paths, herd: &str) -> Option<String> {
    let files = files(paths, herd);
    if !files.key.is_file() {
        return Some(format!(
            "no deploy key at {}; run loopherd merge-guard {herd}",
            files.key.display()
        ));
    }
    let token = std::fs::read_to_string(&files.token).unwrap_or_default();
    token.trim().is_empty().then(|| {
        format!(
            "no token at {}; run loopherd merge-guard {herd} for how to create and store it",
            files.token.display()
        )
    })
}

/// Where gh and git keep the human's GitHub credentials, each with whether
/// it's a directory.
pub fn human_credentials(home: &Path) -> [(PathBuf, bool); 4] {
    [
        (home.join(".config/gh"), true),
        (home.join(".git-credentials"), false),
        (home.join(".config/git/credentials"), false),
        (home.join(".cache/git/credential"), true),
    ]
}
