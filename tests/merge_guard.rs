//! `loopherd merge-guard`: a credentials lead's deploy key and rulesets, set
//! up through a stub `gh` that logs each call and what it read.

mod fakes;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fakes::executable;

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Setup {
    /// Herd `h` on a repo whose origin is acme/widgets, with `lead` as its
    /// `[lead]`, and a `gh` answering the keys and rulesets lists with
    /// `keys` and `rulesets`.
    fn new(lead: &str, keys: &str, rulesets: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let repo = root.join("widgets");
        git(
            &root,
            &["init", "--quiet", "-b", "main", repo.to_str().unwrap()],
        );
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        fs::create_dir_all(root.join("config/herds")).unwrap();
        let herd = format!(
            "repo = {:?}\n[template.t]\nparams = []\nprompt = \"go\"\n[lead]\nprompt = \"go\"\n{lead}\n",
            repo.display().to_string()
        );
        fs::write(root.join("config/herds/h.toml"), herd).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        let calls = root.join("calls");
        executable(
            &root.join("bin/gh"),
            format!(
                "#!/bin/sh\n\
                 echo \"gh $*\" >> {calls}\n\
                 case \"$*\" in\n\
                 *--input*) sed 's/^/  /' >> {calls}; echo '{{}}' ;;\n\
                 'api repos/acme/widgets/keys') echo '{keys}' ;;\n\
                 'api repos/acme/widgets/rulesets?includes_parents=false') echo '{rulesets}' ;;\n\
                 esac\n",
                calls = calls.display()
            ),
        );
        Setup { _dir: dir, root }
    }

    fn run(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .args(["merge-guard", "h"])
            .env("LOOPHERD_CONFIG_DIR", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("HOME", &self.root)
            .env_remove("GH_TOKEN")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .output()
            .unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.root.join("calls")).unwrap_or_default()
    }

    fn key(&self) -> PathBuf {
        self.root.join("config/credentials/h.key")
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

const CREDENTIALS: &str = "merge_guard = \"credentials\"";

const MAIN_RULESET: &str = r#"  {
    "name": "loopherd: default branch",
    "target": "branch",
    "enforcement": "active",
    "conditions": {
      "ref_name": {
        "include": [
          "~DEFAULT_BRANCH"
        ],
        "exclude": []
      }
    },
    "rules": [
      {
        "type": "update"
      },
      {
        "type": "deletion"
      }
    ],
    "bypass_actors": [
      {
        "actor_id": 5,
        "actor_type": "RepositoryRole",
        "bypass_mode": "pull_request"
      }
    ]
  }
"#;

fn force_push_ruleset(bypass: &str) -> String {
    format!(
        r#"  {{
    "name": "loopherd: no force-push",
    "target": "branch",
    "enforcement": "active",
    "conditions": {{
      "ref_name": {{
        "include": [
          "~ALL"
        ],
        "exclude": []
      }}
    }},
    "rules": [
      {{
        "type": "non_fast_forward"
      }}
    ],
    "bypass_actors": {bypass}
  }}
"#
    )
}

#[test]
fn makes_the_key_and_its_ssh_host_adds_it_creates_both_rulesets_and_says_how_to_finish() {
    let setup = Setup::new(CREDENTIALS, "[]", "[]");

    let out = setup.run();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let key = setup.key();
    let mode = fs::metadata(&key).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let public = format!("{}.pub", key.display());
    assert_eq!(
        setup.calls(),
        format!(
            "gh api repos/acme/widgets/keys\n\
             gh repo deploy-key add {public} --repo acme/widgets --allow-write --title loopherd lead-h\n\
             gh api repos/acme/widgets/rulesets?includes_parents=false\n\
             gh api --method POST repos/acme/widgets/rulesets --input -\n\
             {MAIN_RULESET}\
             gh api --method POST repos/acme/widgets/rulesets --input -\n\
             {}",
            force_push_ruleset("[]")
        )
    );
    let ssh_config = setup.root.join("config/ssh/config");
    let mode = fs::metadata(&ssh_config).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "ssh refuses an include others can write"
    );
    assert_eq!(
        fs::read_to_string(&ssh_config).unwrap(),
        format!(
            "Host loopherd-h\n  HostName github.com\n  User git\n  IdentityFile \"{}\"\n  \
             IdentitiesOnly yes\n  IdentityAgent none\n",
            key.display()
        )
    );
    let token = setup.root.join("config/credentials/h.token");
    for line in [
        format!("made the deploy key {}", key.display()),
        format!("wrote Host loopherd-h into {}", ssh_config.display()),
        "first line of ~/.ssh/config".to_string(),
        "   Include ~/config/ssh/config".to_string(),
        "added it to acme/widgets as a write deploy key".to_string(),
        "created ruleset loopherd: default branch".to_string(),
        "created ruleset loopherd: no force-push".to_string(),
        "https://github.com/settings/personal-access-tokens/new".to_string(),
        "Pull requests: Read and write".to_string(),
        "Contents: Read-only".to_string(),
        format!("(umask 077 && cat > {})", token.display()),
        "gh pr merge --admin".to_string(),
    ] {
        assert!(stdout.contains(&line), "missing {line:?} in:\n{stdout}");
    }
}

#[test]
fn a_rerun_keeps_the_key_already_added_and_updates_the_rulesets_in_place() {
    let first = Setup::new(CREDENTIALS, "[]", "[]");
    assert!(first.run().status.success());
    let public = fs::read_to_string(format!("{}.pub", first.key().display())).unwrap();
    let key_text = public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let keys = format!(r#"[{{"id":3,"key":"{key_text}","read_only":false}}]"#);
    let rulesets =
        r#"[{"id":7,"name":"loopherd: default branch"},{"id":8,"name":"loopherd: no force-push"}]"#;
    let setup = Setup::new(&format!("{CREDENTIALS}\nrebase = true"), &keys, rulesets);
    fs::create_dir_all(setup.root.join(".ssh")).unwrap();
    fs::write(
        setup.root.join(".ssh/config"),
        "Include ~/config/ssh/config\nHost github.com\n  User git\n",
    )
    .unwrap();
    fs::create_dir_all(setup.root.join("config/ssh")).unwrap();
    let other = "Host loopherd-other\n  HostName github.com\n";
    let matched = "Match host other.example\n  User me\n";
    fs::write(
        setup.root.join("config/ssh/config"),
        format!("{other}host = loopherd-h\n  HostName old.example\n{matched}"),
    )
    .unwrap();
    fs::create_dir_all(setup.key().parent().unwrap()).unwrap();
    fs::copy(first.key(), setup.key()).unwrap();
    fs::write(format!("{}.pub", setup.key().display()), &public).unwrap();

    let out = setup.run();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bypass = r#"[
      {
        "actor_id": null,
        "actor_type": "DeployKey",
        "bypass_mode": "always"
      }
    ]"#;
    assert_eq!(
        setup.calls(),
        format!(
            "gh api repos/acme/widgets/keys\n\
             gh api repos/acme/widgets/rulesets?includes_parents=false\n\
             gh api --method PUT repos/acme/widgets/rulesets/7 --input -\n\
             {MAIN_RULESET}\
             gh api --method PUT repos/acme/widgets/rulesets/8 --input -\n\
             {}",
            force_push_ruleset(bypass)
        )
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("acme/widgets already has the deploy key"),
        "{stdout}"
    );
    assert!(
        stdout.contains("updated ruleset loopherd: no force-push"),
        "{stdout}"
    );
    assert!(!stdout.contains("~/.ssh/config"), "{stdout}");
    let hosts = fs::read_to_string(setup.root.join("config/ssh/config")).unwrap();
    assert!(hosts.starts_with(other), "{hosts}");
    assert!(hosts.ends_with(matched), "{hosts}");
    assert!(!hosts.contains("old.example"), "{hosts}");
    assert_eq!(hosts.matches("loopherd-h\n").count(), 1, "{hosts}");
}

#[test]
fn a_deploy_key_added_read_only_is_refused_with_how_to_replace_it() {
    let first = Setup::new(CREDENTIALS, "[]", "[]");
    assert!(first.run().status.success());
    let public = fs::read_to_string(format!("{}.pub", first.key().display())).unwrap();
    let key_text = public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let keys = format!(r#"[{{"id":3,"key":"{key_text}","read_only":true}}]"#);
    let setup = Setup::new(CREDENTIALS, &keys, "[]");
    fs::create_dir_all(setup.key().parent().unwrap()).unwrap();
    fs::copy(first.key(), setup.key()).unwrap();
    fs::write(format!("{}.pub", setup.key().display()), &public).unwrap();

    let out = setup.run();

    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd merge-guard: acme/widgets has the deploy key read-only, so the lead can't \
         push; delete it with gh repo deploy-key delete 3 --repo acme/widgets, then run \
         loopherd merge-guard h again\n"
    );
    assert_eq!(setup.calls(), "gh api repos/acme/widgets/keys\n");
}

#[test]
fn a_deploy_key_without_its_public_half_says_how_to_make_it() {
    let setup = Setup::new(CREDENTIALS, "[]", "[]");
    fs::create_dir_all(setup.key().parent().unwrap()).unwrap();
    fs::write(setup.key(), "a private key\n").unwrap();

    let out = setup.run();

    let key = setup.key().display().to_string();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "loopherd merge-guard: the deploy key {key} has no {key}.pub; make it with \
             ssh-keygen -y -f {key} > {key}.pub\n"
        )
    );
    assert_eq!(setup.calls(), "");
}

#[test]
fn a_herd_whose_lead_merges_or_is_guarded_by_hook_is_refused_before_any_call() {
    for (lead, refusal) in [
        (
            "merge = true",
            "herd h's lead has merge on, so merge_guard doesn't apply",
        ),
        (
            "",
            "herd h's lead has merge_guard = \"hook\"; run loopherd herd set h lead.merge_guard \
             '\"credentials\"' first",
        ),
    ] {
        let setup = Setup::new(lead, "[]", "[]");

        let out = setup.run();

        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(refusal), "{stderr}");
        assert_eq!(setup.calls(), "");
        assert!(!setup.key().exists());
    }
}
