//! `loopherd init` through the binary, against a temp repo and a fake `gh`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Answers the gh calls init and the written herd make from JSON fixtures in
/// its own directory, applying `--jq` with jq, and logs each call with its
/// GH_TOKEN.
const FAKE_GH: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "GH_TOKEN=$GH_TOKEN $*" >> "$dir/calls"
filter=.
prev=
for arg in "$@"; do
  [ "$prev" = --jq ] && filter=$arg
  prev=$arg
done
case "$1 $2" in
  "auth token") echo "tok-$4"; exit 0 ;;
  "label create") exit 0 ;;
  "auth status") fixture=auth.json ;;
  "repo view") fixture=repo.json ;;
  "label list") fixture=labels.json ;;
  "issue list") fixture=issues.json ;;
  *) echo "fake gh: unexpected $*" >&2; exit 1 ;;
esac
jq -r "$filter" "$dir/$fixture"
"#;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    /// A repo `widgets` whose origin is `acme/widgets` on GitHub, with gh
    /// logged in as `logins`, no labels and no issues.
    fn new(logins: &[&str]) -> Fixture {
        let fixture = Fixture {
            root: tempfile::tempdir().unwrap(),
        };
        let repo = fixture.repo();
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]);
        git(
            &repo,
            &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
        );
        fs::create_dir_all(fixture.gh_dir()).unwrap();
        let gh = fixture.gh_dir().join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let hosts: Vec<String> = logins
            .iter()
            .map(|login| format!(r#"{{"login": "{login}", "state": "success"}}"#))
            .collect();
        fixture.gh_json(
            "auth.json",
            &format!(r#"{{"hosts": {{"github.com": [{}]}}}}"#, hosts.join(", ")),
        );
        fixture.gh_json("repo.json", r#"{"defaultBranchRef": {"name": "main"}}"#);
        fixture.gh_json("labels.json", "[]");
        fixture.gh_json("issues.json", "[]");
        fs::create_dir_all(fixture.config()).unwrap();
        fs::create_dir_all(fixture.state()).unwrap();
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("widgets")
    }

    fn gh_dir(&self) -> PathBuf {
        self.root.path().join("fake-gh")
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn herds(&self) -> PathBuf {
        self.config().join("herds")
    }

    fn gh_json(&self, name: &str, json: &str) {
        fs::write(self.gh_dir().join(name), json).unwrap();
    }

    fn gh_calls(&self) -> String {
        fs::read_to_string(self.gh_dir().join("calls")).unwrap_or_default()
    }

    fn loopherd(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.gh_dir().display(),
            std::env::var("PATH").unwrap()
        );
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .args(args)
            .current_dir(self.repo())
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.state())
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("GH_TOKEN")
            .output()
            .unwrap()
    }

    fn init(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "init",
            "--check",
            "cargo test",
            "--check",
            "cargo fmt --check",
        ];
        args.extend(extra);
        self.loopherd(&args)
    }

    /// Everything under the config dir, so a refusal can be shown to change nothing.
    fn config_listing(&self) -> Vec<(PathBuf, String)> {
        let mut out = vec![];
        let mut dirs = vec![self.config()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => dirs.push(path),
                    false => out.push((path.clone(), fs::read_to_string(&path).unwrap())),
                }
            }
        }
        out.sort();
        out
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

const ISSUES: &str = r#"[
  {"number": 7, "title": "Add the frobnicator!", "labels": [{"name": "ready-for-agent"}]},
  {"number": 8, "title": "Crash on empty input", "labels": [{"name": "ready-for-agent"}, {"name": "bug"}, {"name": "skill:diagnosing-bugs"}]},
  {"number": 9, "title": "Waits on 7", "labels": [{"name": "ready-for-agent"}, {"name": "blocked"}]},
  {"number": 10, "title": "Which colour?", "labels": [{"name": "needs-info"}, {"name": "ready-for-agent"}]},
  {"number": 11, "title": "Pick a licence", "labels": [{"name": "ready-for-agent"}, {"name": "ready-for-human"}]},
  {"number": 12, "title": "Fix the setup guide's typos in four more places", "labels": [{"name": "documentation"}, {"name": "ready-for-agent"}]}
]"#;

#[test]
fn init_writes_a_herd_whose_feed_lists_the_repos_ready_issues() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("issues.json", ISSUES);

    let out = fixture.init(&[]);

    assert_ok(&out);
    let herds = fixture.loopherd(&["herds"]);
    assert_ok(&herds);
    assert!(
        text(&herds.stdout).contains("loopherd feed widgets  (template issue, max 3)"),
        "{}",
        text(&herds.stdout)
    );
    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert_eq!(
        text(&feed.stdout),
        "would add widgets-7 (template issue)\n\
         would add widgets-8 (template issue)\n\
         would add widgets-12 (template issue)\n\
         widgets: 0 would retire, 3 would add, 0 skipped, 0 errors\n"
    );
}

#[test]
fn the_queue_names_each_issues_branch_type_skill_and_slug_from_its_labels_and_title() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("issues.json", ISSUES);
    assert_ok(&fixture.init(&[]));
    let source = fixture.loopherd(&["herd", "get", "widgets", "queue.source"]);
    let source: String = toml::from_str::<toml::Table>(&format!("v = {}", text(&source.stdout)))
        .unwrap()["v"]
        .as_str()
        .unwrap()
        .into();

    let tasks = Command::new("sh")
        .args(["-c", &source])
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.gh_dir().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert_eq!(
        text(&tasks.stdout),
        "7 feat tdd add-the-frobnicator\n\
         8 fix diagnosing-bugs crash-on-empty-input\n\
         12 docs tdd fix-the-setup-guide-s\n"
    );
    assert!(
        fixture.gh_calls().contains(
            "GH_TOKEN=tok-acme issue list --repo acme/widgets --state open --label ready-for-agent"
        ),
        "{}",
        fixture.gh_calls()
    );
}

#[test]
fn the_worker_prompt_claims_builds_checks_opens_a_closing_pr_and_reports() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();

    for expected in [
        r#"branch = "{type}/{number}-{slug}""#,
        "GH_TOKEN=$(gh auth token --user acme) gh issue view {number} --repo acme/widgets",
        "gh issue comment {number} --repo acme/widgets --body \"claimed by {name}\"",
        "gh issue edit {number} --repo acme/widgets --add-assignee acme",
        "the {skill} skill",
        "cargo test",
        "cargo fmt --check",
        "--base main",
        "Closes #{number}",
        "loopherd report --kind done",
        "--add-label needs-info",
        "\"bug\": \"fix\", \"documentation\": \"docs\"",
    ] {
        assert!(herd.contains(expected), "missing {expected:?} in\n{herd}");
    }
}

#[test]
fn the_herds_done_and_merged_checks_ask_github_as_the_account() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let get = |key: &str| text(&fixture.loopherd(&["herd", "get", "widgets", key]).stdout);

    assert_eq!(
        get("template.issue.done"),
        "\"GH_TOKEN=$(gh auth token --user acme) gh issue view {number} --repo acme/widgets \
         --json state --jq .state | grep -qx CLOSED\"\n"
    );
    assert_eq!(
        get("template.issue.merged"),
        "\"GH_TOKEN=$(gh auth token --user acme) gh pr list --repo acme/widgets --head {branch} \
         --state merged --json number --jq 'length > 0' | grep -qx true\"\n"
    );
}

#[test]
fn the_lead_takes_its_switches_from_lead_defaults_and_a_prompt_file_beside_the_herd() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "[lead_defaults]\nmerge = true\nrebase = true\n",
    )
    .unwrap();

    assert_ok(&fixture.init(&[]));

    let herds = text(&fixture.loopherd(&["herds"]).stdout);
    assert!(
        herds.contains(
            "switches: merge on (herd file), install off (herd file), rebase on (herd file), \
             new_cards off (herd file), design_escalation off (herd file), \
             blocked_dialogs off (herd file)"
        ),
        "{herds}"
    );
    let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
    assert!(lead.contains("{switches}"), "{lead}");
}

#[test]
fn init_prints_the_files_it_wrote_and_the_next_steps() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&[]);

    assert_ok(&out);
    let stdout = text(&out.stdout);
    for expected in [
        format!("wrote {}", fixture.herds().join("widgets.toml").display()),
        format!(
            "wrote {}",
            fixture.herds().join("widgets-lead.md").display()
        ),
        "trust dialog".into(),
        "loopherd herds".into(),
        "loopherd feed widgets --dry-run".into(),
        "loopherd lead widgets".into(),
    ] {
        assert!(
            stdout.contains(&expected),
            "missing {expected:?} in\n{stdout}"
        );
    }
}

#[test]
fn herd_names_the_herd_file() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--herd", "gadgets"]));

    assert!(fixture.herds().join("gadgets.toml").is_file());
    assert!(fixture.herds().join("gadgets-lead.md").is_file());
}

#[test]
fn init_creates_only_the_missing_labels_as_the_account() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json(
        "labels.json",
        r#"[{"name": "bug"}, {"name": "ready-for-agent"}, {"name": "blocked"}]"#,
    );

    let out = fixture.init(&[]);

    assert_ok(&out);
    let creates: Vec<String> = fixture
        .gh_calls()
        .lines()
        .filter(|call| call.contains("label create"))
        .map(|call| call.split(" --description").next().unwrap().to_string())
        .collect();
    assert_eq!(
        creates,
        [
            "GH_TOKEN=tok-acme label create needs-info --repo acme/widgets",
            "GH_TOKEN=tok-acme label create ready-for-human --repo acme/widgets",
        ]
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("created label needs-info"), "{stdout}");
    assert!(stdout.contains("created label ready-for-human"), "{stdout}");
}

#[test]
fn the_account_is_the_logged_in_one_matching_the_remote_owner() {
    let fixture = Fixture::new(&["someone", "Acme"]);

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("gh auth token --user Acme)"), "{herd}");
    assert!(!herd.contains("--user someone"), "{herd}");
}

#[test]
fn without_a_matching_account_gh_account_is_required_and_its_absence_names_the_accounts() {
    let fixture = Fixture::new(&["someone", "other"]);

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: no logged-in gh account matches the remote owner acme \
         (found: someone, other); pass --gh-account <login>\n"
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn gh_account_picks_the_account() {
    let fixture = Fixture::new(&["someone", "other"]);

    assert_ok(&fixture.init(&["--gh-account", "other"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("gh auth token --user other)"), "{herd}");
}

#[test]
fn a_gh_account_that_is_not_logged_in_is_an_error() {
    let fixture = Fixture::new(&["someone"]);

    let out = fixture.init(&["--gh-account", "nobody"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: gh has no logged-in account nobody (found: someone)\n"
    );
}

#[test]
fn a_remote_that_is_not_on_github_is_an_error() {
    let fixture = Fixture::new(&["acme"]);
    git(
        &fixture.repo(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://gitlab.com/acme/widgets.git",
        ],
    );

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: origin https://gitlab.com/acme/widgets.git is not a GitHub repo\n"
    );
}

#[test]
fn an_https_remote_names_the_repo_too() {
    let fixture = Fixture::new(&["acme"]);
    git(
        &fixture.repo(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/widgets",
        ],
    );

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--repo acme/widgets"), "{herd}");
}

#[test]
fn init_refuses_changing_nothing_when_the_herd_file_exists() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    fs::remove_file(fixture.herds().join("widgets-lead.md")).unwrap();
    let before = fixture.config_listing();
    fs::remove_file(fixture.gh_dir().join("calls")).unwrap();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "loopherd init: {} already exists; init never replaces a herd file\n",
            fixture.herds().join("widgets.toml").display()
        )
    );
    assert_eq!(fixture.config_listing(), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn init_refuses_changing_nothing_when_the_lead_prompt_exists() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(fixture.herds().join("widgets-lead.md"), "mine").unwrap();
    let before = fixture.config_listing();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "loopherd init: {} already exists; init never replaces a lead prompt\n",
            fixture.herds().join("widgets-lead.md").display()
        )
    );
    assert_eq!(fixture.config_listing(), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn init_refuses_a_herd_name_that_points_at_another_repo() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(
        fixture.herds().join("widgets.toml"),
        "repo = \"/elsewhere/widgets\"\n",
    )
    .unwrap();
    let before = fixture.config_listing();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: herd widgets already points at /elsewhere/widgets; \
         pass --herd <name> to name this repo's herd\n"
    );
    assert_eq!(fixture.config_listing(), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn init_needs_at_least_one_check() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.loopherd(&["init"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--check"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn origins_over_ssh_or_with_credentials_name_the_repo_too() {
    for url in [
        "ssh://git@github.com/acme/widgets.git",
        "ssh://git@ssh.github.com:443/acme/widgets.git",
        "https://user@github.com/acme/widgets.git",
    ] {
        let fixture = Fixture::new(&["acme"]);
        git(&fixture.repo(), &["remote", "set-url", "origin", url]);

        assert_ok(&fixture.init(&[]));

        let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
        assert!(herd.contains("--repo acme/widgets"), "{url}: {herd}");
    }
}

#[test]
fn existing_labels_match_whatever_their_case() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json(
        "labels.json",
        r#"[{"name": "Ready-For-Agent"}, {"name": "Blocked"}, {"name": "needs-info"}]"#,
    );

    assert_ok(&fixture.init(&[]));

    let calls = fixture.gh_calls();
    let creates: Vec<&str> = calls
        .lines()
        .filter_map(|call| call.split_once("label create "))
        .map(|(_, rest)| rest.split(' ').next().unwrap())
        .collect();
    assert_eq!(creates, ["ready-for-human"]);
}

#[test]
fn the_queue_reads_labels_whatever_their_case_and_skips_unusable_skill_labels() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json(
        "issues.json",
        r#"[
  {"number": 1, "title": "Held", "labels": [{"name": "Ready-For-Agent"}, {"name": "Blocked"}]},
  {"number": 2, "title": "Odd skill", "labels": [{"name": "ready-for-agent"}, {"name": "skill:two words"}, {"name": "Bug"}]},
  {"number": 3, "title": "Empty skill", "labels": [{"name": "ready-for-agent"}, {"name": "skill:"}]}
]"#,
    );
    assert_ok(&fixture.init(&[]));

    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);

    assert_ok(&feed);
    assert_eq!(
        text(&feed.stdout),
        "would add widgets-2 (template issue)\n\
         would add widgets-3 (template issue)\n\
         widgets: 0 would retire, 2 would add, 0 skipped, 0 errors\n"
    );
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("ascii_downcase"), "{herd}");
}

#[test]
fn a_repo_without_a_default_branch_is_an_error_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("repo.json", r#"{"defaultBranchRef": null}"#);

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: acme/widgets has no default branch yet; push a first commit\n"
    );
    assert!(!fixture.gh_calls().contains("label create"));
    assert!(!fixture.herds().join("widgets.toml").exists());
}

#[test]
fn a_check_naming_a_placeholder_of_init_stays_literal() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.loopherd(&["init", "--check", "echo @SWITCHES@ {x}"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("`echo @SWITCHES@ {{x}}`"), "{herd}");
}

#[test]
fn a_herd_name_with_a_control_character_is_refused() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--herd", "x\ny"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: \"x\\ny\" is not a herd name\n"
    );
}

#[test]
fn a_dangling_symlink_at_the_herd_file_is_refused_before_any_label_is_created() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    std::os::unix::fs::symlink("/nonexistent", fixture.herds().join("widgets.toml")).unwrap();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fixture.gh_calls(), "");
}
