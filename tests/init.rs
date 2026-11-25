//! `loopherd init` through the binary, against a temp repo and a fake `gh`.

mod fakes;

use fakes::executable;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Answers the gh calls init and the written herd make from JSON fixtures in
/// its own directory, applying `--jq` with jq, and logs each call with its
/// GH_TOKEN. An `auth-plain.out` or `.err` file makes it a gh without
/// `auth status --json`, printing that file on stdout or stderr instead, and
/// exiting with the code in `auth-plain.status`, if any.
const FAKE_GH: &str = r#"#!/bin/sh
ulimit -S -f unlimited
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
  "auth status")
    if [ -f "$dir/auth-fails" ]; then cat "$dir/auth-fails" >&2; exit 1; fi
    if [ -f "$dir/auth-plain.out" ] || [ -f "$dir/auth-plain.err" ]; then
      case " $* " in
        *" --json "*) printf 'unknown flag: --json\n\nUsage:  gh auth status [flags]\n' >&2; exit 1 ;;
      esac
      [ -f "$dir/auth-plain.out" ] && cat "$dir/auth-plain.out"
      [ -f "$dir/auth-plain.err" ] && cat "$dir/auth-plain.err" >&2
      exit "$(cat "$dir/auth-plain.status" 2>/dev/null || echo 0)"
    fi
    fixture=auth.json ;;
  "repo view") fixture=repo.json ;;
  "label list") fixture=labels.json ;;
  "issue list") fixture=issues.json ;;
  *) echo "fake gh: unexpected $*" >&2; exit 1 ;;
esac
jq -r "$filter" "$dir/$fixture"
"#;

struct Fixture {
    root: TempDir,
    dir: String,
}

impl Fixture {
    /// A repo `widgets` whose origin is `acme/widgets` on GitHub, with gh
    /// logged in as `logins`, no labels and no issues.
    fn new(logins: &[&str]) -> Fixture {
        Fixture::in_dir("widgets", logins)
    }

    /// As [`Fixture::new`], with the repo in a directory named `dir`.
    fn in_dir(dir: &str, logins: &[&str]) -> Fixture {
        let fixture = Fixture {
            root: tempfile::tempdir().unwrap(),
            dir: dir.into(),
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
        executable(&gh, FAKE_GH);
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
        self.root.path().join(&self.dir)
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
        self.run(Command::new(env!("CARGO_BIN_EXE_loopherd")).args(args))
    }

    /// `loopherd init` under a file-size limit that `file` outgrows but every
    /// file init writes before it fits; the limit comes from what init writes here.
    fn init_failing_to_write(&self, file: &str) -> Output {
        assert_ok(&self.init(&[]));
        let order = ["widgets-lead.md", "widgets-grill.md", "widgets.toml"];
        let sizes: Vec<u64> = order
            .iter()
            .map(|name| fs::metadata(self.herds().join(name)).unwrap().len())
            .collect();
        let at = order.iter().position(|name| *name == file).unwrap();
        fs::remove_dir_all(self.herds()).unwrap();
        // ulimit -f counts 512-byte blocks.
        let blocks = match sizes[..at].iter().max() {
            None => (sizes[at] - 1) / 512,
            Some(earlier) => earlier.div_ceil(512),
        };
        assert!(
            sizes[at] > blocks * 512,
            "{file} ({} bytes) fits in the {blocks} blocks the files before it need",
            sizes[at]
        );
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                r#"ulimit -S -f "$1"; trap '' XFSZ; shift; exec "$@""#,
                "sh",
                &blocks.to_string(),
                env!("CARGO_BIN_EXE_loopherd"),
                "init",
            ])
            .args(Fixture::CHECKS);
        self.run(&mut command)
    }

    /// Runs `command` in the repo unless it names its own directory.
    fn run(&self, command: &mut Command) -> Output {
        if command.get_current_dir().is_none() {
            command.current_dir(self.repo());
        }
        let path = format!(
            "{}:{}",
            self.gh_dir().display(),
            std::env::var("PATH").unwrap()
        );
        isolate_git_config(command)
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.state())
            .env("CLAUDE_CONFIG_DIR", self.config().join("claude"))
            .env_remove("HERDR_PANE_ID")
            .env_remove("LOOPHERD_BROKER_OF")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("GH_TOKEN")
            .output()
            .unwrap()
    }

    const CHECKS: [&str; 4] = ["--check", "cargo test", "--check", "cargo fmt --check"];

    fn init(&self, extra: &[&str]) -> Output {
        let mut args = vec!["init"];
        args.extend(Fixture::CHECKS);
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

/// Hides the user's, system and caller's git config, whose `safe.directory = *`
/// (set on GitHub runners) would stop git refusing a repo it is told to distrust.
/// A test's own `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` is kept.
fn isolate_git_config(command: &mut Command) -> &mut Command {
    for inherited in ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"] {
        if !command.get_envs().any(|(key, _)| key == inherited) {
            command.env_remove(inherited);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    isolate_git_config(&mut command).current_dir(dir);
    command
}

fn git(dir: &Path, args: &[&str]) {
    let status = git_command(dir).args(args).status().unwrap();
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
fn the_herd_file_opens_by_naming_the_github_repo_its_issues_come_from() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.starts_with("# Herd widgets: one worker per ready GitHub issue in acme/widgets.\n"),
        "{herd}"
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
        "GH_TOKEN=$(gh auth token --user acme) gh issue view {number} --repo acme/widgets \
         --comments",
        "GH_TOKEN=$(gh auth token --user acme) gh issue comment {number} --repo acme/widgets \
         --body \"claimed by {name}\"",
        "GH_TOKEN=$(gh auth token --user acme) gh issue edit {number} --repo acme/widgets \
         --add-assignee acme",
        "GH_TOKEN=$(gh auth token --user acme) gh pr create --repo acme/widgets",
        "GH_TOKEN=$(gh auth token --user acme) gh issue edit {number} --repo acme/widgets \
         --add-label needs-info",
        "the {skill} skill",
        "cargo test",
        "cargo fmt --check",
        "--base main",
        "Closes #{number}",
        "loopherd report --kind done",
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
             blocked_dialogs off (herd file), follow_ups_ready off (herd file)"
        ),
        "{herds}"
    );
    let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
    assert!(lead.contains("{switches}"), "{lead}");
    assert!(
        lead.contains("Your review-round limit: {review_rounds}."),
        "{lead}"
    );
    assert!(
        lead.contains(
            "re-arm after every exit but `{herd} drained`: on that, report done and\n   \
             stop. On `<member> merged`, run `loopherd feed {herd}`,\n   \
             which retires it once its issue is closed. On `<name> claimable`, feed."
        ),
        "{lead}"
    );
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
        format!(
            "wrote {}",
            fixture.herds().join("widgets-grill.md").display()
        ),
        "trust dialog".into(),
        "init never accepts it for you".into(),
        "\"Setting up a project\" in `loopherd --skill`".into(),
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
        "loopherd init: origin https://gitlab.com/acme/widgets.git is not a GitHub repo; \
         pass --repo <owner>/<name>\n"
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
            "loopherd init: {} already exists; init never replaces a lead prompt; pass --no-lead to write the herd without a lead\n",
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
fn origins_over_ssh_or_with_credentials_name_the_repo_too() {
    for url in [
        "ssh://git@github.com/acme/widgets.git",
        "ssh://git@ssh.github.com:443/acme/widgets.git",
        "https://user@github.com/acme/widgets.git",
        "https://www.github.com/acme/widgets",
        "git@GitHub.com:acme/widgets.git",
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
        "loopherd init: \"x\\ny\" can't name a herd: it needs a lowercase letter then at most \
         26 of a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>\n"
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

#[test]
fn a_dangling_herds_link_is_refused_before_any_label_is_created() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.config()).unwrap();
    std::os::unix::fs::symlink("/nonexistent", fixture.herds()).unwrap();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "loopherd init: {} is a dangling symlink; init won't write through it\n",
            fixture.herds().display()
        )
    );
    assert!(
        !fixture.gh_calls().contains("label "),
        "{}",
        fixture.gh_calls()
    );
}

fn assert_refused(fixture: &Fixture, out: &Output, stderr: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert_eq!(text(&out.stderr), stderr);
    assert!(!fixture.herds().exists());
}

fn herd_repo(fixture: &Fixture, herd: &str) -> String {
    let file = fs::read_to_string(fixture.herds().join(format!("{herd}.toml"))).unwrap();
    toml::from_str::<toml::Table>(&file).unwrap()["repo"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn a_repo_dir_that_cant_name_a_lead_and_members_is_refused_before_any_gh_call() {
    for dir in ["MyApp", "foo.github.io", "2048", &"a".repeat(28)] {
        let fixture = Fixture::in_dir(dir, &["acme"]);

        let out = fixture.init(&[]);

        assert_refused(
            &fixture,
            &out,
            &format!(
                "loopherd init: {dir:?} can't name a herd: it needs a lowercase letter then at \
                 most 26 of a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>\n"
            ),
        );
        assert_eq!(fixture.gh_calls(), "", "{dir}");
    }
}

#[test]
fn herd_names_a_herd_for_a_repo_dir_that_cant() {
    let fixture = Fixture::in_dir("MyApp", &["acme"]);

    assert_ok(&fixture.init(&["--herd", &"a".repeat(27)]));
}

#[test]
fn an_ssh_host_alias_origin_needs_repo() {
    let fixture = Fixture::new(&["acme"]);
    git(
        &fixture.repo(),
        &[
            "remote",
            "set-url",
            "origin",
            "git@github-personal:acme/widgets.git",
        ],
    );

    assert_refused(
        &fixture,
        &fixture.init(&[]),
        "loopherd init: origin git@github-personal:acme/widgets.git is not a GitHub repo; \
         pass --repo <owner>/<name>\n",
    );
    assert_ok(&fixture.init(&["--repo", "acme/widgets"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--repo acme/widgets"), "{herd}");
}

#[test]
fn a_repo_without_an_origin_needs_repo() {
    let fixture = Fixture::new(&["acme"]);
    git(&fixture.repo(), &["remote", "remove", "origin"]);

    assert_refused(
        &fixture,
        &fixture.init(&[]),
        &format!(
            "loopherd init: {} has no origin remote; pass --repo <owner>/<name>\n",
            fixture.repo().display()
        ),
    );
    assert_ok(&fixture.init(&["--repo", "acme/widgets"]));
}

#[test]
fn an_owner_repo_or_login_outside_githubs_characters_is_refused() {
    let fixture = Fixture::new(&["acme"]);
    git(
        &fixture.repo(),
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:ac$me/widgets.git",
        ],
    );
    assert_refused(
        &fixture,
        &fixture.init(&[]),
        "loopherd init: origin git@github.com:ac$me/widgets.git is not a GitHub repo; \
         pass --repo <owner>/<name>\n",
    );
    assert_refused(
        &fixture,
        &fixture.init(&["--repo", "acme/wid gets"]),
        "loopherd init: --repo \"acme/wid gets\" is not <owner>/<name>\n",
    );

    let fixture = Fixture::new(&["ac me"]);
    assert_refused(
        &fixture,
        &fixture.init(&["--gh-account", "ac me"]),
        "loopherd init: gh has no logged-in account; run `gh auth login`\n",
    );
}

#[test]
fn gh_account_matches_whatever_its_case() {
    let fixture = Fixture::new(&["someone", "other"]);

    assert_ok(&fixture.init(&["--gh-account", "OTHER"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("gh auth token --user other)"), "{herd}");
}

#[test]
fn without_a_logged_in_gh_account_init_says_to_log_in() {
    let fixture = Fixture::new(&[]);

    assert_refused(
        &fixture,
        &fixture.init(&[]),
        "loopherd init: gh has no logged-in account; run `gh auth login`\n",
    );
}

#[test]
fn the_logins_come_from_auth_status_json_where_gh_has_it() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&[]));

    let calls = fixture.gh_calls();
    assert!(calls.contains("auth status --json hosts"), "{calls}");
    assert!(!calls.contains("--hostname"), "{calls}");
}

#[test]
fn on_a_gh_without_auth_status_json_the_logins_come_from_its_account_lines() {
    let fixture = Fixture::new(&[]);
    fixture.gh_json(
        "auth-plain.out",
        "github.com\n  \u{2713} Logged in to github.com account someone (keyring)\n  \
         - Active account: true\n  - Git operations protocol: ssh\n\n  \
         \u{2713} Logged in to github.com account Acme (keyring)\n  - Active account: false\n",
    );

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("gh auth token --user Acme)"), "{herd}");
    let calls = fixture.gh_calls();
    assert!(
        calls.contains("auth status --hostname github.com"),
        "{calls}"
    );
}

#[test]
fn on_an_older_gh_the_logins_come_from_its_logged_in_as_lines_on_stderr() {
    let fixture = Fixture::new(&[]);
    fixture.gh_json(
        "auth-plain.err",
        "github.com\n  \u{2713} Logged in to github.com as someone (oauth_token)\n  \
         \u{2713} Git operations for github.com configured to use https protocol.\n  \
         \u{2713} Token: gho_************************************\n",
    );

    assert_ok(&fixture.init(&["--gh-account", "someone"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("gh auth token --user someone)"), "{herd}");
}

#[test]
fn the_plain_auth_status_fallback_keeps_the_login_filter_and_errors() {
    let fixture = Fixture::new(&[]);
    fixture.gh_json(
        "auth-plain.out",
        "  \u{2713} Logged in to github.com account .hidden (keyring)\n  \
         \u{2713} Logged in to github.com account -flag (keyring)\n  \
         \u{2713} Logged in to github.com account someone (keyring)\n",
    );

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: no logged-in gh account matches the remote owner acme \
         (found: someone); pass --gh-account <login>\n"
    );
}

#[test]
fn the_plain_auth_status_fallback_without_a_login_says_to_log_in() {
    let fixture = Fixture::new(&[]);
    fixture.gh_json(
        "auth-plain.err",
        "You are not logged into any GitHub hosts. To log in, run: gh auth login\n",
    );
    fixture.gh_json("auth-plain.status", "1");

    assert_refused(
        &fixture,
        &fixture.init(&[]),
        "loopherd init: gh has no logged-in account; run `gh auth login`\n",
    );
}

#[test]
fn a_gh_auth_status_failure_other_than_the_unknown_flag_is_its_own_error() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("auth-fails", "error connecting to api.github.com\n");

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("error connecting to api.github.com"),
        "{stderr}"
    );
    assert!(!fixture.gh_calls().contains("--hostname"));
}

#[test]
fn init_refuses_a_second_herd_for_the_same_repo() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let before = fixture.config_listing();
    fs::remove_file(fixture.gh_dir().join("calls")).unwrap();

    let out = fixture.init(&["--herd", "other"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "loopherd init: herd widgets already runs {}; init writes one herd per repo\n",
            fixture.repo().display()
        )
    );
    assert_eq!(fixture.config_listing(), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn a_failed_herd_file_write_leaves_neither_file_and_a_rerun_succeeds() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_failing_to_write("widgets.toml");

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).starts_with(&format!(
            "loopherd init: write {}",
            fixture.herds().join("widgets.toml").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
    assert_ok(&fixture.init(&[]));
}

#[test]
fn a_failed_lead_prompt_write_leaves_no_file() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_failing_to_write("widgets-lead.md");

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).starts_with(&format!(
            "loopherd init: write {}",
            fixture.herds().join("widgets-lead.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn a_failed_grill_prompt_write_leaves_no_file() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_failing_to_write("widgets-grill.md");

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).starts_with(&format!(
            "loopherd init: write {}",
            fixture.herds().join("widgets-grill.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn init_refuses_changing_nothing_when_the_grill_prompt_exists() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(fixture.herds().join("widgets-grill.md"), "mine").unwrap();
    let before = fixture.config_listing();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        format!(
            "loopherd init: {} already exists; init never replaces a grill prompt\n",
            fixture.herds().join("widgets-grill.md").display()
        )
    );
    assert_eq!(fixture.config_listing(), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn a_repo_path_with_a_quote_is_written_as_a_toml_string() {
    let fixture = Fixture::in_dir("wid\"gets", &["acme"]);

    assert_ok(&fixture.init(&["--herd", "widgets"]));

    assert_eq!(
        herd_repo(&fixture, "widgets"),
        fixture.repo().to_string_lossy()
    );
}

#[test]
fn a_check_with_a_triple_quote_is_refused() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.loopherd(&["init", "--check", "echo '''"]);

    assert_refused(
        &fixture,
        &out,
        "loopherd init: \"echo '''\" can't go in the worker prompt: it has ''' or a control \
         character\n",
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn init_from_a_subdirectory_points_the_herd_at_the_repo_root() {
    let fixture = Fixture::new(&["acme"]);
    let sub = fixture.repo().join("src/deep");
    fs::create_dir_all(&sub).unwrap();

    assert_ok(
        &fixture.run(
            Command::new(env!("CARGO_BIN_EXE_loopherd"))
                .args(["init", "--check", "cargo test"])
                .current_dir(&sub),
        ),
    );

    assert_eq!(
        herd_repo(&fixture, "widgets"),
        fixture.repo().to_string_lossy()
    );
}

#[test]
fn a_herd_name_with_an_uppercase_letter_or_a_space_after_the_first_is_refused() {
    for herd in ["mYApp", "x y"] {
        let fixture = Fixture::new(&["acme"]);

        assert_refused(
            &fixture,
            &fixture.init(&["--herd", herd]),
            &format!(
                "loopherd init: {herd:?} can't name a herd: it needs a lowercase letter then at \
                 most 26 of a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>\n"
            ),
        );
    }
}

#[test]
fn a_repo_name_starting_with_a_dot_is_accepted_from_repo() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--repo", "acme/.github"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("GitHub issue #{number} of acme/.github"),
        "{herd}"
    );
    assert!(herd.contains("--repo acme/.github"), "{herd}");
}

#[test]
fn a_repo_name_starting_with_a_dot_is_accepted_from_origin() {
    let fixture = Fixture::new(&["acme"]);
    git(
        &fixture.repo(),
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:acme/.github.git",
        ],
    );

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("GitHub issue #{number} of acme/.github"),
        "{herd}"
    );
    assert!(herd.contains("--repo acme/.github"), "{herd}");
}

#[test]
fn an_owner_name_starting_with_a_dot_is_accepted() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--repo", ".acme/widgets", "--gh-account", "acme"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--repo .acme/widgets"), "{herd}");
}

#[test]
fn an_origin_that_is_a_path_step_once_its_git_suffix_is_stripped_is_refused() {
    for url in ["git@github.com:acme/..git", "git@github.com:acme/...git"] {
        let fixture = Fixture::new(&["acme"]);
        git(&fixture.repo(), &["remote", "set-url", "origin", url]);

        assert_refused(
            &fixture,
            &fixture.init(&[]),
            &format!(
                "loopherd init: origin {url} is not a GitHub repo; pass --repo <owner>/<name>\n"
            ),
        );
        assert_eq!(fixture.gh_calls(), "", "{url}");
    }
}

#[test]
fn a_gh_login_starting_with_a_dot_is_not_offered() {
    let fixture = Fixture::new(&[".acme"]);

    assert_refused(
        &fixture,
        &fixture.init(&["--gh-account", ".acme"]),
        "loopherd init: gh has no logged-in account; run `gh auth login`\n",
    );
}

#[test]
fn repo_refuses_a_name_that_is_a_path_step_an_option_or_has_a_git_suffix() {
    for slug in [
        "acme/.",
        "acme/..",
        "acme/-widgets",
        "-acme/widgets",
        "./widgets",
        "../widgets",
        "acme/widgets.git",
    ] {
        let fixture = Fixture::new(&["acme"]);

        assert_refused(
            &fixture,
            &fixture.init(&[&format!("--repo={slug}")]),
            &format!("loopherd init: --repo {slug:?} is not <owner>/<name>\n"),
        );
        assert_eq!(fixture.gh_calls(), "", "{slug}");
    }
}

#[test]
fn init_refuses_while_a_herd_file_it_cant_read_could_run_this_repo() {
    for (contents, mode) in [
        ("repo = [", 0o644),
        ("repo = \"/x\"\nunknown = 1\n", 0o644),
        ("repo = \"/x\"\n", 0o000),
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::create_dir_all(fixture.herds()).unwrap();
        let broken = fixture.herds().join("old.toml");
        fs::write(&broken, contents).unwrap();
        fs::set_permissions(&broken, fs::Permissions::from_mode(mode)).unwrap();

        let out = fixture.init(&[]);

        assert_eq!(out.status.code(), Some(1), "{contents}");
        assert!(
            text(&out.stderr).starts_with(&format!(
                "loopherd init: can't tell which repo {} runs; fix or remove it",
                broken.display()
            )),
            "{}",
            text(&out.stderr)
        );
        assert_eq!(fixture.gh_calls(), "");
        assert!(!fixture.herds().join("widgets.toml").exists());
    }
}

#[test]
fn a_toml_directory_or_dangling_symlink_among_the_herds_doesnt_stop_init() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds().join("dir.toml")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", fixture.herds().join("x.toml")).unwrap();

    assert_ok(&fixture.init(&[]));
}

fn repo_file(fixture: &Fixture, path: &str) -> String {
    fs::read_to_string(fixture.repo().join(path)).unwrap()
}

#[test]
fn without_a_tracker_doc_init_writes_githubs_and_the_default_labels_uncommitted() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&[]);

    assert_ok(&out);

    let tracker = repo_file(&fixture, "docs/agents/issue-tracker.md");
    assert!(
        tracker.starts_with("# Issue tracker: GitHub\n"),
        "{tracker}"
    );
    assert!(tracker.contains("`gh` CLI"), "{tracker}");
    let labels = repo_file(&fixture, "docs/agents/triage-labels.md");
    for role in [
        "needs-triage",
        "needs-info",
        "ready-for-agent",
        "ready-for-human",
        "wontfix",
    ] {
        assert!(
            labels.contains(&format!("| `{role}` | `{role}` |")),
            "missing {role} in\n{labels}"
        );
    }
    let log = git_command(&fixture.repo())
        .args(["rev-list", "--all", "--count"])
        .output()
        .unwrap();
    assert_eq!(text(&log.stdout), "0\n");
    let status = git_command(&fixture.repo())
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    assert_eq!(
        text(&status.stdout),
        "?? AGENTS.md\n?? docs/agents/issue-tracker.md\n?? docs/agents/loopherd.md\n\
         ?? docs/agents/shepherd.md\n?? docs/agents/triage-labels.md\n"
    );
    assert!(
        text(&out.stdout).contains(
            "wrote docs/agents/issue-tracker.md (uncommitted)\n\
             wrote docs/agents/triage-labels.md (uncommitted)\n"
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn an_existing_doc_naming_another_tracker_gets_a_placeholder_source_herds_reports_invalid() {
    let fixture = Fixture::new(&["acme"]);
    let doc = "# Issue tracker: Jira\n\nIssues live in the WID project.\n";
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(fixture.repo().join("docs/agents/issue-tracker.md"), doc).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(&format!(
            "Fill in from docs/agents/issue-tracker.md, in {}: [queue] source",
            fixture.herds().join("widgets.toml").display()
        )),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(repo_file(&fixture, "docs/agents/issue-tracker.md"), doc);
    assert!(!fixture.repo().join("docs/agents/triage-labels.md").exists());
    assert!(
        !fixture.gh_calls().contains("label "),
        "{}",
        fixture.gh_calls()
    );
    let herds = fixture.loopherd(&["herds"]);
    assert_eq!(herds.status.code(), Some(1));
    assert!(
        text(&herds.stdout).contains("invalid widgets.toml: queue: source is empty"),
        "{}",
        text(&herds.stdout)
    );
    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);
    assert_ne!(feed.status.code(), Some(0));
}

#[test]
fn an_existing_github_doc_gets_the_github_snippets_and_is_left_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("issues.json", ISSUES);
    let doc = "# Issue tracker: GitHub\n\nOur own words.\n";
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(fixture.repo().join("docs/agents/issue-tracker.md"), doc).unwrap();

    assert_ok(&fixture.init(&[]));

    assert_eq!(repo_file(&fixture, "docs/agents/issue-tracker.md"), doc);
    assert!(!fixture.repo().join("docs/agents/triage-labels.md").exists());
    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert!(
        text(&feed.stdout).ends_with("widgets: 0 would retire, 3 would add, 0 skipped, 0 errors\n"),
        "{}",
        text(&feed.stdout)
    );
}

#[test]
fn triage_labels_name_the_claimable_filter_the_question_label_and_the_created_labels() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json(
        "issues.json",
        r#"[
  {"number": 1, "title": "Go", "labels": [{"name": "AFK"}]},
  {"number": 2, "title": "Asked", "labels": [{"name": "AFK"}, {"name": "Q:Info"}]},
  {"number": 3, "title": "Mine", "labels": [{"name": "AFK"}, {"name": "hfk"}]},
  {"number": 4, "title": "Waits", "labels": [{"name": "AFK"}, {"name": "blocked"}]},
  {"number": 5, "title": "Default name means nothing here", "labels": [{"name": "AFK"}, {"name": "needs-info"}]}
]"#,
    );
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(
        fixture.repo().join("docs/agents/triage-labels.md"),
        "# Triage Labels\n\n\
         | Label in mattpocock/skills | Label in our tracker | Meaning |\n\
         | --- | --- | --- |\n\
         | `needs-triage` | `triage` | x |\n\
         | `needs-info` | `Q:Info` | x |\n\
         | `ready-for-agent` | `AFK` | x |\n\
         | `ready-for-human` | `HFK` | x |\n",
    )
    .unwrap();

    assert_ok(&fixture.init(&[]));

    let calls = fixture.gh_calls();
    for label in ["AFK", "Q:Info", "HFK", "blocked"] {
        assert!(
            calls.contains(&format!("label create {label} --repo acme/widgets")),
            "{calls}"
        );
    }
    assert!(!calls.contains("label create ready-for-agent"), "{calls}");
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--add-label Q:Info"), "{herd}");
    assert!(herd.contains("--label AFK"), "{herd}");
    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert_eq!(
        text(&feed.stdout),
        "would add widgets-1 (template issue)\n\
         would add widgets-5 (template issue)\n\
         widgets: 0 would retire, 2 would add, 0 skipped, 0 errors\n"
    );
}

#[test]
fn a_triage_label_init_cant_put_in_a_shell_command_or_blocked_is_refused_before_any_gh_call() {
    for (role, label, message) in [
        (
            "needs-info",
            "needs info",
            "maps needs-info to \"needs info\"",
        ),
        ("needs-info", "-x", "maps needs-info to \"-x\""),
        (
            "ready-for-agent",
            "AFK ready",
            "maps ready-for-agent to \"AFK ready\"",
        ),
        (
            "ready-for-human",
            "$(x)",
            "maps ready-for-human to \"$(x)\"",
        ),
        (
            "needs-info",
            "Blocked",
            "maps needs-info to blocked, which is loopherd's own label",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
        fs::write(
            fixture.repo().join("docs/agents/triage-labels.md"),
            format!("| `{role}` | `{label}` | x |\n"),
        )
        .unwrap();

        let out = fixture.init(&[]);

        assert_eq!(out.status.code(), Some(1), "{role} {label}");
        assert!(text(&out.stderr).contains(message), "{}", text(&out.stderr));
        assert_eq!(fixture.gh_calls(), "");
        assert!(!fixture.herds().join("widgets.toml").exists());
    }
}

#[test]
fn tracker_names_the_doc_init_writes_when_there_is_none() {
    for (args, title, valid, github) in [
        (
            &["--tracker", "github"][..],
            "# Issue tracker: GitHub\n",
            true,
            true,
        ),
        (
            &["--tracker", "local"][..],
            "# Issue tracker: Local Markdown\n",
            true,
            false,
        ),
        (
            &[
                "--tracker",
                "other",
                "--tracker-desc",
                "Issues live in Linear, team WID.",
            ][..],
            "# Issue tracker\n\nIssues live in Linear, team WID.\n",
            false,
            false,
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);

        assert_ok(&fixture.init(args));

        let doc = repo_file(&fixture, "docs/agents/issue-tracker.md");
        assert!(doc.starts_with(title), "{args:?}: {doc}");
        assert!(
            repo_file(&fixture, "docs/agents/triage-labels.md")
                .contains("| `ready-for-agent` | `ready-for-agent` |")
        );
        let herds = fixture.loopherd(&["herds"]);
        assert_eq!(
            herds.status.code(),
            Some(if valid { 0 } else { 1 }),
            "{args:?}"
        );
        assert_eq!(
            fixture.gh_calls().contains("label create"),
            github,
            "{args:?}: {}",
            fixture.gh_calls()
        );
    }
}

#[test]
fn the_github_and_local_tracker_docs_cover_wayfinding_blocking_and_sub_issues() {
    for (tracker, sub_issue) in [
        ("github", "**Make an issue a sub-issue of a parent**"),
        ("local", "- **Child ticket**: "),
    ] {
        let fixture = Fixture::new(&["acme"]);

        assert_ok(&fixture.init(&["--tracker", tracker]));

        let doc = repo_file(&fixture, "docs/agents/issue-tracker.md");
        assert!(
            doc.contains("\n## Wayfinding operations\n"),
            "{tracker}: {doc}"
        );
        assert!(doc.contains("\n- **Blocking**: "), "{tracker}: {doc}");
        assert!(doc.contains(sub_issue), "{tracker}: {doc}");
    }
}

#[test]
fn an_existing_triage_labels_doc_is_kept_when_init_writes_the_tracker_doc() {
    let fixture = Fixture::new(&["acme"]);
    let labels = "| `ready-for-agent` | `AFK` | x |\n";
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(fixture.repo().join("docs/agents/triage-labels.md"), labels).unwrap();

    assert_ok(&fixture.init(&[]));

    assert_eq!(repo_file(&fixture, "docs/agents/triage-labels.md"), labels);
    assert!(fixture.repo().join("docs/agents/issue-tracker.md").exists());
}

#[test]
fn tracker_flags_that_cant_apply_are_refused_before_any_change() {
    for (args, stderr) in [
        (
            &["--tracker", "other"][..],
            "loopherd init: --tracker other needs --tracker-desc <text>\n",
        ),
        (
            &["--tracker", "other", "--tracker-desc", "  "][..],
            "loopherd init: the tracker description is empty\n",
        ),
        (
            &["--tracker-desc", "Linear"][..],
            "loopherd init: --tracker-desc goes with --tracker other\n",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);

        assert_refused(&fixture, &fixture.init(args), stderr);
        assert!(!fixture.repo().join("docs").exists(), "{args:?}");
    }
}

#[test]
fn tracker_is_refused_when_the_repo_already_has_a_tracker_doc() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(
        fixture.repo().join("docs/agents/issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();

    assert_refused(
        &fixture,
        &fixture.init(&["--tracker", "local"]),
        "loopherd init: docs/agents/issue-tracker.md already names the tracker; drop --tracker\n",
    );
}

#[test]
fn a_tracker_doc_init_cant_write_leaves_no_herd_files_and_a_rerun_succeeds() {
    let fixture = Fixture::new(&["acme"]);
    let agents = fixture.repo().join("docs/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o555)).unwrap();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(&format!(
            "write {}",
            agents.join("issue-tracker.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ok(&fixture.init(&[]));
}

#[test]
fn a_placeholder_herd_for_one_repo_doesnt_stop_init_in_another() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let other = fixture.root.path().join("gadgets");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    git(
        &other,
        &["remote", "add", "origin", "git@github.com:acme/gadgets.git"],
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    command
        .args(["init", "--check", "cargo test"])
        .current_dir(&other);

    assert_ok(&fixture.run(&mut command));
}

fn write_docs(fixture: &Fixture, tracker: Option<&str>, labels: &str) {
    let dir = fixture.repo().join("docs/agents");
    fs::create_dir_all(&dir).unwrap();
    if let Some(tracker) = tracker {
        fs::write(dir.join("issue-tracker.md"), tracker).unwrap();
    }
    fs::write(dir.join("triage-labels.md"), labels).unwrap();
}

#[test]
fn a_local_tracker_refuses_a_label_its_shell_snippets_cant_hold() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(
        &fixture,
        Some("# Issue tracker: Local Markdown\n"),
        "| `ready-for-human` | `$(x)` | x |\n",
    );

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("maps ready-for-human to \"$(x)\""),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().join("widgets.toml").exists());
}

#[test]
fn a_local_tracker_reads_its_triage_labels_names() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(&[
        (".scratch/f/issues/01-a.md", "Status: AFK\n"),
        (".scratch/f/issues/02-b.md", "Status: AFK, Asked\n"),
        (".scratch/f/issues/03-c.md", "Status: ready-for-agent\n"),
    ]);
    write_docs(
        &fixture,
        Some("# Issue tracker: Local Markdown\n"),
        "| `ready-for-agent` | `AFK` | x |\n| `needs-info` | `asked` | x |\n",
    );

    assert_ok(&fixture.init(&[]));

    let feed = fixture.loopherd_with(&env, &["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert!(
        text(&feed.stdout).starts_with(
            "would add widgets-f-01 (template issue)\nwidgets: 0 would retire, 1 would add"
        ),
        "{}",
        text(&feed.stdout)
    );
    assert!(
        fixture
            .herd_get("template.issue.prompt")
            .contains("`Status:` line to `asked`")
    );
}

#[test]
fn a_tracker_other_than_github_takes_labels_no_shell_command_runs() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(
        &fixture,
        Some("# Issue tracker: Jira\n"),
        "| `needs-info` | `Needs {more} info` | x |\n",
    );

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("mark\ntask {number} Needs {{more}} info (fill in)"),
        "{herd}"
    );
}

#[test]
fn triage_label_roles_match_whatever_their_case() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(&fixture, None, "| `Needs-Info` | `asked` | x |\n");

    assert_ok(&fixture.init(&[]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--add-label asked"), "{herd}");
}

#[test]
fn two_roles_mapped_to_one_label_are_refused_before_any_gh_call() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(
        &fixture,
        None,
        "| `ready-for-agent` | `todo` | x |\n| `needs-info` | `TODO` | x |\n",
    );

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(
            "docs/agents/triage-labels.md maps ready-for-agent and needs-info to one label, todo"
        ),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn with_a_placeholder_source_the_next_steps_fill_it_before_feeding() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--tracker", "other", "--tracker-desc", "Linear"]);

    assert_ok(&out);
    let stdout = text(&out.stdout);
    let next = &stdout[stdout.find("Next:").unwrap()..];
    assert!(
        next.contains(&format!(
            "  2. Fill in from docs/agents/issue-tracker.md, in {}: [queue] source",
            fixture.herds().join("widgets.toml").display()
        )),
        "{next}"
    );
    assert!(
        next.contains(&format!(
            "  4. Fill in from docs/agents/issue-tracker.md, in {}: [template.grill] done, \
             and in {} each line marked (fill in).",
            fixture.herds().join("widgets.toml").display(),
            fixture.herds().join("widgets-grill.md").display()
        )),
        "{next}"
    );
    assert!(
        next.contains("  7. Tailor the herd to the repo per \"Setting up a project\""),
        "{next}"
    );
    assert!(
        next.contains("  8. Review and commit what init wrote in the repo"),
        "{next}"
    );
    assert!(next.contains("  11. loopherd lead widgets"), "{next}");
}

#[test]
fn a_github_title_in_any_case_after_other_text_or_a_bom_gets_the_github_snippets() {
    for doc in [
        "# Issue Tracker: GitHub\n",
        "# issue tracker: github\n",
        "\u{feff}# Issue tracker: GitHub\n",
        "<!-- set up by hand -->\n\n# Issue tracker: GitHub\n",
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
        fs::write(fixture.repo().join("docs/agents/issue-tracker.md"), doc).unwrap();

        assert_ok(&fixture.init(&[]));

        assert_ok(&fixture.loopherd(&["herds"]));
        assert!(fixture.gh_calls().contains("label create"), "{doc:?}");
    }
}

impl Fixture {
    /// `loopherd init --check 'cargo test'` on a terminal, typing Enter for the
    /// repo and herd name, then `input`, then Enter for the rest. `input`
    /// answers the tracker question, or the GitHub one when a flag skips it.
    fn init_on_terminal(&self, extra: &[&str], input: &str) -> Output {
        let mut args = vec!["--check", "cargo test"];
        args.extend(extra);
        self.on_terminal(&args, &format!("\n\n{input}\n\n\n\n\n\n\n\n\n"))
    }

    /// `loopherd init <args>` with stdin and stderr on a terminal, typing `input`.
    fn on_terminal(&self, args: &[&str], input: &str) -> Output {
        self.on_terminal_with(&[], args, input)
    }

    /// As [`Fixture::on_terminal`], with `envs` set.
    fn on_terminal_with(&self, envs: &[(&str, &str)], args: &[&str], input: &str) -> Output {
        let typed = self.root.path().join("typed");
        fs::write(&typed, input).unwrap();
        let mut line = format!("'{}' init", env!("CARGO_BIN_EXE_loopherd"));
        for arg in args {
            line.push_str(&format!(" '{arg}'"));
        }
        let mut command = Command::new("script");
        command
            .args(["-qec", &line, "/dev/null"])
            .env("SHELL", "/bin/sh")
            .envs(envs.iter().copied())
            .stdin(fs::File::open(typed).unwrap());
        self.run(&mut command)
    }
}

#[test]
fn on_a_terminal_init_asks_for_the_tracker_until_answered_then_for_an_others_description() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "bogus\nother\n\nIssues live in Linear.\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert_eq!(
        shown
            .matches("Where does this repo track issues? [github/local/other] (github): ")
            .count(),
        2,
        "{shown}"
    );
    assert!(shown.contains("answer github, local or other"), "{shown}");
    assert_eq!(
        shown
            .matches("Describe how this repo tracks issues, in one paragraph: ")
            .count(),
        2,
        "{shown}"
    );
    assert_eq!(
        repo_file(&fixture, "docs/agents/issue-tracker.md"),
        "# Issue tracker\n\nIssues live in Linear.\n"
    );
    assert_eq!(fixture.loopherd(&["herds"]).status.code(), Some(1));
}

#[test]
fn on_a_terminal_an_empty_answer_takes_github() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("(github): "),
        "{}",
        text(&out.stdout)
    );
    assert!(
        repo_file(&fixture, "docs/agents/issue-tracker.md")
            .starts_with("# Issue tracker: GitHub\n")
    );
}

#[test]
fn on_a_terminal_tracker_answers_the_question() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&["--tracker", "local"], "acme/widgets");

    assert_ok(&out);
    assert!(
        fixture
            .herd_get("template.issue.merged")
            .contains("--repo acme/widgets")
    );
    assert!(
        !text(&out.stdout).contains("Where does"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        repo_file(&fixture, "docs/agents/issue-tracker.md")
            .starts_with("# Issue tracker: Local Markdown\n")
    );
}

fn herd_toml(fixture: &Fixture) -> toml::Table {
    toml::from_str(&fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap()).unwrap()
}

#[test]
fn without_check_init_takes_cargos_checks_from_cargo_toml() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains(
            "   - `cargo fmt --check`\n   - `cargo clippy --all-targets -- -D warnings`\n   - `cargo test`\n"
        ),
        "{herd}"
    );
}

#[test]
fn without_check_init_takes_package_jsons_lint_and_test_scripts() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("package.json"),
        r#"{"scripts": {"build": "tsc", "test": "vitest", "lint": "eslint ."}}"#,
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("   - `npm run lint`\n   - `npm test`\n"),
        "{herd}"
    );
}

#[test]
fn without_check_or_a_build_file_init_names_the_flag() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("package.json"),
        r#"{"scripts": {"build": "tsc"}}"#,
    )
    .unwrap();

    let out = fixture.loopherd(&["init"]);

    assert_refused(
        &fixture,
        &out,
        "loopherd init: no checks found in Cargo.toml or package.json; pass --check <cmd>\n",
    );
}

#[test]
fn max_sets_how_many_workers_run_at_once() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--max", "5"]));

    assert_eq!(herd_toml(&fixture)["queue"]["max"].as_integer(), Some(5));
    assert!(text(&fixture.loopherd(&["herds"]).stdout).contains("max 5"),);
}

#[test]
fn a_max_that_isnt_a_positive_integer_is_refused() {
    for max in ["0", "-1", "three"] {
        let fixture = Fixture::new(&["acme"]);

        let out = fixture.init(&[&format!("--max={max}")]);

        assert_eq!(out.status.code(), Some(2), "{max}");
        assert!(text(&out.stderr).contains("--max"), "{}", text(&out.stderr));
        assert!(!fixture.herds().exists());
    }
}

#[test]
fn no_lead_writes_no_lead_table_or_prompt_and_no_lead_step() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--no-lead"]);

    assert_ok(&out);
    assert!(!herd_toml(&fixture).contains_key("lead"));
    assert!(!fixture.herds().join("widgets-lead.md").exists());
    assert!(!text(&out.stdout).contains("lead"), "{}", text(&out.stdout));
    assert_ok(&fixture.loopherd(&["herds"]));
}

#[test]
fn no_lead_ignores_an_existing_lead_prompt() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(fixture.herds().join("widgets-lead.md"), "mine\n").unwrap();

    assert_ok(&fixture.init(&["--no-lead"]));

    assert_eq!(
        fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap(),
        "mine\n"
    );
}

#[test]
fn context_limit_sets_the_workers_handoff_limit() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--context-limit", "120000"]));

    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_integer(),
        Some(120000)
    );
    assert_ok(&fixture.loopherd(&["herds"]));
}

#[test]
fn a_percentage_context_limit_is_written_and_read_back_by_diff() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--context-limit", "50%"]));

    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_str(),
        Some("50%")
    );
    assert_ok(&fixture.loopherd(&["herds"]));
    let out = fixture.init(&["--diff"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
}

#[test]
fn a_context_limit_in_k_is_written_as_tokens() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--context-limit", "120k"]));

    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_integer(),
        Some(120000)
    );
}

#[test]
fn without_context_limit_the_template_leaves_it_unset() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&[]));

    assert!(
        !herd_toml(&fixture)["template"]["issue"]
            .as_table()
            .unwrap()
            .contains_key("context_limit")
    );
}

#[test]
fn a_context_limit_over_80_percent_of_the_window_names_the_cap() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "context_window = 100000\n",
    )
    .unwrap();

    let out = fixture.init(&["--context-limit", "80001"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: context_limit 80001 is over 80000, 80% of the 100000-token \
         context window\n"
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn a_context_limit_under_the_minimum_is_refused() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--context-limit", "9999"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("context_limit must be at least 10000"),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn on_a_terminal_init_shows_each_inferred_value_takes_edits_and_re_asks_bad_ones() {
    let fixture = Fixture::new(&["acme", "bot"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();
    let typed = [
        "",        // repo
        "gadgets", // herd name
        "",        // tracker
        "",        // owner/name
        "",        // use the suggested checks
        "0", "4", // max
        "n", // lead
        "900000", "100000", // context_limit
        "bot",    // gh account
        "trunk",  // default branch
        "",       // groom
        "",       // confirm
    ];

    let out = fixture.on_terminal(&[], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    let shown = text(&out.stdout);
    for expected in [
        format!("Repo ({}): ", fixture.repo().display()),
        "Herd name (widgets): ".into(),
        "GitHub repo (owner/name) (acme/widgets): ".into(),
        "  cargo fmt --check\r\n  cargo clippy --all-targets -- -D warnings\r\n  cargo test\r\n"
            .into(),
        "Use these checks? [Y/n]: ".into(),
        "max, how many workers run at once (3): ".into(),
        "max must be a positive integer, got \"0\"".into(),
        "Give the herd a lead, a member that feeds, watches and verifies its workers? [Y/n]: "
            .into(),
        "Workers' handoff limit in tokens or a percentage of the window (context_limit); \
         Enter leaves it unset, so the default 250000 applies: "
            .into(),
        "context_limit 900000 is over 800000, 80% of the 1000000-token context window".into(),
        "gh account (logged in: acme, bot) (acme): ".into(),
        "Default branch (main): ".into(),
        "Herd gadgets:".into(),
        "  max     4 workers at once".into(),
        "  lead    no".into(),
        "  limit   100000".into(),
        "  checks  cargo fmt --check; cargo clippy --all-targets -- -D warnings; cargo test".into(),
        "Write this herd? [Y/n]: ".into(),
    ] {
        assert!(
            shown.contains(&expected),
            "missing {expected:?} in\n{shown}"
        );
    }
    let herd = fs::read_to_string(fixture.herds().join("gadgets.toml")).unwrap();
    let table: toml::Table = toml::from_str(&herd).unwrap();
    assert_eq!(table["queue"]["max"].as_integer(), Some(4));
    assert_eq!(
        table["template"]["issue"]["context_limit"].as_integer(),
        Some(100000)
    );
    assert!(!table.contains_key("lead"));
    assert!(herd.contains("--base trunk"), "{herd}");
    assert!(herd.contains("gh auth token --user bot"), "{herd}");
    assert!(!fixture.herds().join("gadgets-lead.md").exists());
}

#[test]
fn on_a_terminal_declining_the_summary_writes_nothing() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\n\n\nn\n");

    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stdout).contains("nothing written"));
    assert!(!fixture.herds().exists());
    assert!(!fixture.gh_calls().contains("label create"));
    assert!(!fixture.repo().join("AGENTS.md").exists());
    assert!(!fixture.repo().join("docs").exists());
}

#[test]
fn on_a_terminal_without_a_build_file_init_asks_for_checks_until_one_is_given() {
    let fixture = Fixture::new(&["acme"]);
    let typed = "\n\n\n\n\nmake lint\nmake test\n\n\n\n\n\n\n\n\n\n";

    let out = fixture.on_terminal(&[], typed);

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert_eq!(
        shown
            .matches("A check workers run before calling work done: ")
            .count(),
        2,
        "{shown}"
    );
    assert!(!shown.contains("Use these checks?"), "{shown}");
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("   - `make lint`\n   - `make test`\n"),
        "{herd}"
    );
}

#[test]
fn on_a_terminal_declining_the_suggested_checks_edits_each_then_adds_more() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();
    let typed = [
        "",
        "",
        "",
        "",  // repo, herd name, owner/name, tracker
        "n", // use the suggested checks
        "",  // keep cargo fmt --check
        "-", // drop clippy
        "cargo nextest run",
        "just check",
        "", // no more checks
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ];

    let out = fixture.on_terminal(&[], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("Check (cargo clippy --all-targets -- -D warnings): "),
        "{}",
        text(&out.stdout)
    );
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("   - `cargo fmt --check`\n   - `cargo nextest run`\n   - `just check`\n5."),
        "{herd}"
    );
}

#[test]
fn a_max_over_what_a_herd_file_holds_is_refused() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--max", "9223372036854775808"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(!fixture.herds().exists());
}

#[test]
fn npm_inits_placeholder_test_script_is_not_suggested() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("package.json"),
        r#"{"scripts": {"lint": "eslint .", "test": "echo \"Error: no test specified\" && exit 1"}}"#,
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("   - `npm run lint`\n5."), "{herd}");
}

#[test]
fn on_a_terminal_a_taken_herd_name_or_lead_prompt_is_asked_again() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(
        fixture.herds().join("widgets.toml"),
        "repo = \"/elsewhere\"\n",
    )
    .unwrap();
    fs::write(fixture.herds().join("gadgets-lead.md"), "mine\n").unwrap();
    let typed = ["", "", "gadgets", "", "", "", "y", "n", "", "", "", "", ""];

    let out = fixture.on_terminal(&["--check", "cargo test"], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("herd widgets already points at /elsewhere"),
        "{shown}"
    );
    assert!(
        shown.contains(
            "gadgets-lead.md already exists; init never replaces a lead prompt; answer n"
        ),
        "{shown}"
    );
    assert!(fixture.herds().join("gadgets.toml").exists());
    assert_eq!(
        fs::read_to_string(fixture.herds().join("gadgets-lead.md")).unwrap(),
        "mine\n"
    );
}

#[test]
fn on_a_terminal_a_branch_that_cant_go_in_the_prompt_is_asked_again() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\nma\x27\x27\x27in\ntrunk\n\n");

    assert_ok(&out);
    assert!(text(&out.stdout).contains("can't go in the worker prompt"));
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--base trunk"), "{herd}");
}

#[test]
fn on_a_terminal_an_unmatched_account_is_asked_from_the_logged_in_ones() {
    let fixture = Fixture::new(&["bot", "other"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\nnobody\nother\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("gh account (logged in: bot, other): "),
        "{shown}"
    );
    assert!(
        shown.contains("gh has no logged-in account nobody (found: bot, other)"),
        "{shown}"
    );
    assert!(fixture.gh_calls().contains("auth token --user other"));
}

#[test]
fn on_a_terminal_flags_answer_their_questions() {
    let fixture = Fixture::new(&["acme"]);

    let out =
        fixture.init_on_terminal(&["--max", "2", "--no-lead", "--context-limit", "50000"], "");

    assert_ok(&out);
    assert!(fixture.herds().join("widgets.toml").exists());
    let shown = text(&out.stdout);
    for question in ["max,", "Give the herd a lead", "handoff limit"] {
        assert!(!shown.contains(question), "{question} in\n{shown}");
    }
    assert!(shown.contains("  max     2 workers at once"), "{shown}");
}

#[test]
fn on_a_terminal_a_repo_another_herd_runs_is_refused_before_the_name_is_asked() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let out = fixture.on_terminal(&[], "\nx\ny\n");

    assert_eq!(out.status.code(), Some(1));
    let shown = text(&out.stdout);
    assert!(
        shown.contains(&format!(
            "herd widgets already runs {}; init writes one herd per repo",
            fixture.repo().display()
        )),
        "{shown}"
    );
    assert!(!shown.contains("Herd name"), "{shown}");
}

#[test]
fn on_a_terminal_an_unreadable_herd_file_is_refused_before_the_name_is_asked() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(fixture.herds().join("broken.toml"), "not toml").unwrap();

    let out = fixture.on_terminal(&[], "\nx\ny\n");

    assert_eq!(out.status.code(), Some(1));
    let shown = text(&out.stdout);
    assert!(shown.contains("can't tell which repo"), "{shown}");
    assert!(!shown.contains("Herd name"), "{shown}");
    assert!(!fixture.herds().join("widgets.toml").exists());
}

#[test]
fn on_a_terminal_the_limit_question_and_summary_show_config_tomls_default() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "default_context_limit = 90000\n",
    )
    .unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("Enter leaves it unset, so config.toml's 90000 applies: "),
        "{shown}"
    );
    assert!(
        shown.contains("  limit   unset, config.toml's 90000 applies"),
        "{shown}"
    );
}

#[test]
fn on_a_terminal_the_summary_says_an_unset_limit_takes_the_default() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("  limit   unset, the default 250000 applies"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn on_a_terminal_a_limit_of_exactly_80_percent_is_taken_and_one_more_asked_again() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n800001\n800000\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert_eq!(
        shown.matches("context_limit 800001 is over 800000").count(),
        1,
        "{shown}"
    );
    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_integer(),
        Some(800000)
    );
}

#[test]
fn a_context_limit_of_exactly_80_percent_of_the_window_is_taken() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "context_window = 100000\n",
    )
    .unwrap();

    assert_ok(&fixture.init(&["--context-limit", "80000"]));

    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_integer(),
        Some(80000)
    );
}

#[test]
fn on_a_terminal_a_branch_with_whitespace_is_asked_again() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\nma in\ntrunk\n\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("\"ma in\" can't name a branch"),
        "{}",
        text(&out.stdout)
    );
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--base trunk"), "{herd}");
}

#[test]
fn on_a_terminal_a_repo_answer_outside_git_is_asked_again_naming_no_flag() {
    let fixture = Fixture::new(&["acme"]);
    let outside = fixture.root.path().join("fake-gh");

    let out = fixture.on_terminal(
        &["--check", "cargo test"],
        &format!("{}\n\n\n\n\n\n\n\n\n\n\n\n\n", outside.display()),
    );

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains(&format!(
            "can't read a git repo at {}: git ",
            outside.display()
        )),
        "{shown}"
    );
    assert!(!shown.contains("--repo"), "{shown}");
}

#[test]
fn on_a_terminal_a_max_too_big_for_a_herd_file_says_so() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n9223372036854775808\n4\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout)
            .contains("max 9223372036854775808 is too big; at most 9223372036854775807"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(herd_toml(&fixture)["queue"]["max"].as_integer(), Some(4));
}

#[test]
fn a_package_json_init_cant_parse_is_warned_about_and_suggests_nothing() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("package.json"), "{\"scripts\": ").unwrap();

    let out = fixture.loopherd(&["init"]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "warning: can't read {}, so it suggests no checks",
            fixture.repo().join("package.json").display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("pass --check <cmd>"), "{stderr}");
}

#[test]
fn a_package_json_with_a_byte_order_mark_suggests_its_scripts() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("package.json"),
        "\u{feff}{\"scripts\": {\"test\": \"vitest\"}}",
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("   - `npm test`\n5."), "{herd}");
}

#[test]
fn a_null_lint_script_suggests_nothing() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("package.json"),
        r#"{"scripts": {"lint": null, "test": "vitest"}}"#,
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(!herd.contains("npm run lint"), "{herd}");
}

#[test]
fn cargo_toml_and_package_json_both_suggest_checks() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();
    fs::write(
        fixture.repo().join("package.json"),
        r#"{"scripts": {"test": "vitest"}}"#,
    )
    .unwrap();

    assert_ok(&fixture.loopherd(&["init"]));

    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(
        herd.contains("   - `cargo test`\n   - `npm test`\n5."),
        "{herd}"
    );
}

#[test]
fn on_a_terminal_the_summary_names_the_labels_and_docs_writing_creates() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(
            "Writing it also creates the labels ready-for-agent, needs-info, ready-for-human \
             and blocked in acme/widgets where missing, and writes \
             docs/agents/issue-tracker.md, docs/agents/triage-labels.md, \
             docs/agents/loopherd.md, docs/agents/shepherd.md and AGENTS.md in the repo, \
             uncommitted.\r\n"
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_repo_git_refuses_to_read_is_refused_with_gits_reason() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.run(
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .args(["init", "--check", "cargo test"])
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
    );

    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "can't read a git repo at {}; run init inside one",
            fixture.repo().display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("safe.directory"), "{stderr}");
    assert!(!stderr.contains("--repo"), "{stderr}");
    assert!(!stderr.contains("not in a git repo"), "{stderr}");
}

#[test]
fn on_a_terminal_a_repo_answer_git_refuses_to_read_shows_gits_reason() {
    let fixture = Fixture::new(&["acme"]);
    let other = fixture.root.path().join("other");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    let repo = fixture.repo().to_string_lossy().into_owned();

    let out = fixture.on_terminal_with(
        &[
            ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", &repo),
        ],
        &["--check", "cargo test"],
        &format!("{}\n\n\n\n\n\n\n\n\n\n\n\n\n", other.display()),
    );

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains(&format!("can't read a git repo at {}: ", other.display())),
        "{shown}"
    );
    assert!(shown.contains("safe.directory"), "{shown}");
}

#[test]
fn git_refusals_are_seen_under_a_global_and_system_safe_directory_wildcard() {
    let root = tempfile::tempdir().unwrap();
    let trusting = root.path().join("gitconfig");
    fs::write(&trusting, "[safe]\n\tdirectory = *\n").unwrap();

    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_repo_git_refuses_to_read_is_refused_with_gits_reason",
            "on_a_terminal_a_repo_answer_git_refuses_to_read_shows_gits_reason",
        ])
        .env("GIT_CONFIG_GLOBAL", &trusting)
        .env("GIT_CONFIG_SYSTEM", &trusting)
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .output()
        .unwrap();

    let stdout = text(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("2 passed"),
        "stdout:\n{stdout}\nstderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn git_refusals_are_seen_under_an_inherited_command_line_safe_directory_wildcard() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_repo_git_refuses_to_read_is_refused_with_gits_reason",
            "on_a_terminal_a_repo_answer_git_refuses_to_read_shows_gits_reason",
        ])
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", "*")
        .env("GIT_CONFIG_PARAMETERS", "'safe.directory'='*'")
        .output()
        .unwrap();

    let stdout = text(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("2 passed"),
        "stdout:\n{stdout}\nstderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn a_cargo_only_repo_warns_about_nothing() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();

    let out = fixture.loopherd(&["init"]);

    assert_ok(&out);
    assert!(
        !text(&out.stderr).contains("warning:"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_repo_with_no_build_file_warns_about_none() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.loopherd(&["init"]);

    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("pass --check <cmd>"), "{stderr}");
    assert!(!stderr.contains("warning:"), "{stderr}");
}

#[test]
fn on_a_terminal_a_configured_window_alone_leaves_the_default_limit() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "context_window = 1000000\n",
    )
    .unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("  limit   unset, the default 250000 applies"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn on_a_terminal_a_configured_limit_over_the_cap_names_the_cap() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "default_context_limit = 900000\n",
    )
    .unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(
            "  limit   unset, the cap of 800000 (80% of the largest known 1000000-token context \
             window) applies"
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn on_a_terminal_a_default_limit_over_a_configured_windows_cap_names_that_window() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "context_window = 100000\n",
    )
    .unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(
            "so the cap of 80000 (80% of config.toml's 100000-token context window) applies: "
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn in_a_fresh_repo_init_writes_agents_md_pointing_at_the_herd_docs() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&[]);

    assert_ok(&out);
    let agents = repo_file(&fixture, "AGENTS.md");
    assert!(agents.contains("## loopherd\n"), "{agents}");
    for doc in ["docs/agents/loopherd.md", "docs/agents/issue-tracker.md"] {
        assert!(
            agents.contains(&format!("`{doc}`")),
            "missing {doc} in\n{agents}"
        );
    }
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(
            "wrote docs/agents/loopherd.md (uncommitted)\nwrote docs/agents/shepherd.md \
             (uncommitted)\nwrote AGENTS.md (uncommitted)\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Review and commit what init wrote in the repo: docs/agents/issue-tracker.md, \
             docs/agents/triage-labels.md, docs/agents/loopherd.md, docs/agents/shepherd.md \
             and AGENTS.md."
        ),
        "{stdout}"
    );
}

#[test]
fn the_loopherd_doc_covers_claiming_branches_checks_the_closing_pr_questions_and_reports() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(&fixture, None, "| `needs-info` | `Q:Info` | x |\n");

    assert_ok(&fixture.init(&[]));

    let doc = repo_file(&fixture, "docs/agents/loopherd.md");
    for expected in [
        "`docs/agents/issue-tracker.md`",
        "`claimed by <member>`",
        "assign it to acme",
        "`<type>/<number>-<slug>`",
        "- `cargo test`\n- `cargo fmt --check`\n",
        "`Closes #<number>`",
        "--base main",
        "`Q:Info`",
        "loopherd report --kind done",
        "loopherd report --kind question",
    ] {
        assert!(doc.contains(expected), "missing {expected:?} in\n{doc}");
    }
}

#[test]
fn an_agents_md_without_a_loopherd_section_gets_one_appended() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("AGENTS.md"), "# Widgets\n\nRun `make`.").unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    let agents = repo_file(&fixture, "AGENTS.md");
    assert!(
        agents.starts_with("# Widgets\n\nRun `make`.\n\n## loopherd\n"),
        "{agents}"
    );
    assert!(agents.contains("`docs/agents/loopherd.md`"), "{agents}");
    assert!(
        text(&out.stdout).contains("added a loopherd section to AGENTS.md (uncommitted)\n"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn an_agents_md_with_a_loopherd_section_is_left_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    let agents = "# Widgets\n\n### Working with LoopHerd\n\nOur own words.\n";
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
    assert!(
        !text(&out.stdout).contains("AGENTS.md"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn init_writes_a_shepherd_runbook_stub_pointing_at_loopherd_docs_runbook() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&[]);

    assert_ok(&out);
    let stub = repo_file(&fixture, "docs/agents/shepherd.md");
    assert!(stub.contains("`loopherd docs runbook`"), "{stub}");
    assert!(stub.contains("`loopherd --skill`"), "{stub}");
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("wrote docs/agents/shepherd.md (uncommitted)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Fill in docs/agents/shepherd.md, the shepherd's runbook for this repo; \
             `loopherd docs runbook` says what each section holds."
        ),
        "{stdout}"
    );
}

#[test]
fn an_existing_shepherd_runbook_is_left_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(fixture.repo().join("docs/agents/shepherd.md"), "ours\n").unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(repo_file(&fixture, "docs/agents/shepherd.md"), "ours\n");
    let stdout = text(&out.stdout);
    assert!(!stdout.contains("docs/agents/shepherd.md"), "{stdout}");
}

#[test]
fn diff_leaves_a_filled_in_shepherd_runbook_alone() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let path = fixture.repo().join("docs/agents/shepherd.md");
    fs::write(
        &path,
        "# Shepherd runbook\n\nFeed with `loopherd feed widgets`.\n",
    )
    .unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_shows_a_missing_shepherd_runbook_as_new() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let path = fixture.repo().join("docs/agents/shepherd.md");
    fs::remove_file(&path).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let header = format!("--- /dev/null\n+++ {} (init now)\n", path.display());
    assert!(stdout.contains(&header), "{stdout}");
    assert!(stdout.contains("+# Shepherd runbook\n"), "{stdout}");
}

#[test]
fn an_existing_loopherd_doc_is_left_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(fixture.repo().join("docs/agents/loopherd.md"), "ours\n").unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(repo_file(&fixture, "docs/agents/loopherd.md"), "ours\n");
    assert!(
        !text(&out.stdout).contains("wrote docs/agents/loopherd.md"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn an_agents_md_init_cant_append_to_is_left_as_it_was_with_no_other_file() {
    let fixture = Fixture::new(&["acme"]);
    let agents = fixture.repo().join("AGENTS.md");
    fs::write(&agents, "# Widgets\n").unwrap();
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o444)).unwrap();

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(&format!("write {}", agents.display())),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(repo_file(&fixture, "AGENTS.md"), "# Widgets\n");
    assert!(!fixture.repo().join("docs").exists());
    assert!(!fixture.herds().exists());
    assert!(!fixture.gh_calls().contains("label create"));
}

#[test]
fn on_a_terminal_the_summary_names_a_section_added_to_an_existing_agents_md() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("AGENTS.md"), "# Widgets\n").unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(
            "docs/agents/loopherd.md, docs/agents/shepherd.md and a loopherd section in AGENTS.md \
             in the repo, uncommitted."
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn loopherd_in_a_code_block_is_not_a_loopherd_section() {
    let fixture = Fixture::new(&["acme"]);
    let agents = "# Widgets\n\n```sh\n# install loopherd first\n```\n\n    # loopherd too\n";
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    assert_ok(&fixture.init(&[]));

    let written = repo_file(&fixture, "AGENTS.md");
    assert!(
        written.starts_with(agents) && written.contains("\n## loopherd\n"),
        "{written}"
    );
}

#[test]
fn a_symlinked_agents_md_is_left_alone_with_a_step_to_add_the_section() {
    let fixture = Fixture::new(&["acme"]);
    let outside = fixture.root.path().join("global.md");
    fs::write(&outside, "# Mine\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.repo().join("AGENTS.md")).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "# Mine\n");
    assert!(
        text(&out.stdout).contains(
            "AGENTS.md is a symlink, so init left it alone; add a loopherd section pointing \
             at docs/agents/loopherd.md and docs/agents/issue-tracker.md to it by hand."
        ),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn an_unreadable_loopherd_doc_is_left_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    let doc = fixture.repo().join("docs/agents/loopherd.md");
    fs::create_dir_all(doc.parent().unwrap()).unwrap();
    fs::write(&doc, "ours\n").unwrap();
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o000)).unwrap();

    let out = fixture.init(&[]);

    fs::set_permissions(&doc, fs::Permissions::from_mode(0o644)).unwrap();
    assert_ok(&out);
    assert_eq!(fs::read_to_string(&doc).unwrap(), "ours\n");
}

#[test]
fn a_failed_append_truncates_agents_md_back_and_removes_the_files_and_dirs_init_created() {
    let fixture = Fixture::new(&["acme"]);
    // Past the limit below by less than the section, and bigger than every other file.
    let blocks = 64;
    let agents = format!("# Widgets\n\n{}\n", "x".repeat(blocks * 512 - 30));
    fs::write(fixture.repo().join("AGENTS.md"), &agents).unwrap();

    let out = init_with_file_limit(&fixture, blocks);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
    assert!(!fixture.repo().join("docs").exists());
    assert!(!fixture.herds().exists());
}

#[test]
fn a_failed_append_keeps_the_dirs_that_were_there_before_init() {
    let fixture = Fixture::new(&["acme"]);
    let blocks = 64;
    let agents = format!("# Widgets\n\n{}\n", "x".repeat(blocks * 512 - 30));
    fs::write(fixture.repo().join("AGENTS.md"), &agents).unwrap();
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();

    let out = init_with_file_limit(&fixture, blocks);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
    assert!(fixture.herds().is_dir());
    assert!(fixture.repo().join("docs/agents").is_dir());
}

/// Runs init with files capped at `blocks` 512-byte blocks.
fn init_with_file_limit(fixture: &Fixture, blocks: usize) -> std::process::Output {
    fixture.run(
        Command::new("sh")
            .args([
                "-c",
                r#"ulimit -S -f "$1"; trap '' XFSZ; shift; exec "$@""#,
                "sh",
                &blocks.to_string(),
                env!("CARGO_BIN_EXE_loopherd"),
                "init",
            ])
            .args(Fixture::CHECKS),
    )
}

#[test]
fn a_loopherd_doc_that_would_land_outside_the_repo_is_left_to_a_step() {
    for (link, target) in [("docs", "outside_docs"), ("docs/agents", "outside_agents")] {
        let fixture = Fixture::new(&["acme"]);
        let outside = fixture.root.path().join(target);
        fs::create_dir_all(&outside).unwrap();
        // The tracker doc is already there, so only loopherd.md would go through the link.
        let agents_dir = match link {
            "docs" => outside.join("agents"),
            _ => outside.clone(),
        };
        let link_path = fixture.repo().join(link);
        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &link_path).unwrap();
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(
            agents_dir.join("issue-tracker.md"),
            "# Issue tracker: GitHub\n",
        )
        .unwrap();

        let out = fixture.init(&[]);

        assert_ok(&out);
        assert!(!agents_dir.join("loopherd.md").exists(), "{link}");
        let stdout = text(&out.stdout);
        assert!(
            stdout.contains(
                "docs/agents/loopherd.md would land outside the repo through a symlink, so \
                 init didn't write it; write it in the repo by hand."
            ),
            "{link}: {stdout}"
        );
        assert!(
            !stdout.contains("wrote docs/agents/loopherd.md"),
            "{link}: {stdout}"
        );
    }
}

#[test]
fn tracker_docs_that_would_land_outside_the_repo_are_left_to_steps() {
    for (link, target) in [("docs", "outside_docs"), ("docs/agents", "outside_agents")] {
        let fixture = Fixture::new(&["acme"]);
        let outside = fixture.root.path().join(target);
        fs::create_dir_all(&outside).unwrap();
        let link_path = fixture.repo().join(link);
        fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &link_path).unwrap();

        let out = fixture.init(&[]);

        assert_ok(&out);
        let stdout = text(&out.stdout);
        for doc in ["issue-tracker.md", "triage-labels.md"] {
            let found = walkdir(&outside);
            assert!(!found.iter().any(|f| f.ends_with(doc)), "{link}: {found:?}");
            assert!(
                stdout.contains(&format!(
                    "docs/agents/{doc} would land outside the repo through a symlink, so \
                     init didn't write it; write it in the repo by hand."
                )),
                "{link}: {stdout}"
            );
            assert!(
                !stdout.contains(&format!("wrote docs/agents/{doc}")),
                "{link}: {stdout}"
            );
        }
    }
}

#[test]
fn repo_docs_that_would_land_in_the_git_dir_are_left_to_steps() {
    let fixture = Fixture::new(&["acme"]);
    let info = fixture.repo().join(".git/info");
    fs::create_dir_all(&info).unwrap();
    fs::create_dir_all(fixture.repo().join("docs")).unwrap();
    std::os::unix::fs::symlink(&info, fixture.repo().join("docs/agents")).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    let stdout = text(&out.stdout);
    for doc in ["issue-tracker.md", "triage-labels.md", "loopherd.md"] {
        assert!(!info.join(doc).exists(), "{doc}");
        assert!(
            stdout.contains(&format!("docs/agents/{doc} would land outside the repo")),
            "{stdout}"
        );
    }
}

/// Every file under `dir`, recursively.
fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = vec![];
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => files.extend(walkdir(&path)),
            false => files.push(path),
        }
    }
    files
}

#[test]
fn a_setext_heading_naming_loopherd_is_a_loopherd_section() {
    for agents in [
        "# Widgets\n\nLoopherd\n========\n\nOurs.\n",
        "# Widgets\n\nWorking with\nloopherd\n---\n",
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

        assert_ok(&fixture.init(&[]));

        assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
    }
}

#[test]
fn a_fence_closes_only_on_a_run_of_its_character_as_long_as_the_opener() {
    let agents = "# Widgets\n\n````md\n```\n````` x\n    ````\n## loopherd\n```\n~~~~\n````\n";
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    assert_ok(&fixture.init(&[]));

    let written = repo_file(&fixture, "AGENTS.md");
    assert!(
        written.starts_with(agents) && written.len() > agents.len(),
        "{written}"
    );
}

#[test]
fn a_crlf_agents_md_gets_a_crlf_section() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("AGENTS.md"), "# Widgets\r\n").unwrap();

    assert_ok(&fixture.init(&[]));

    let written = repo_file(&fixture, "AGENTS.md");
    assert!(
        written.starts_with("# Widgets\r\n\r\n## loopherd\r\n"),
        "{written:?}"
    );
    assert!(!written.replace("\r\n", "").contains('\n'), "{written:?}");
}

#[test]
fn a_dangling_agents_md_symlink_is_left_alone_with_a_step() {
    let fixture = Fixture::new(&["acme"]);
    let gone = fixture.root.path().join("gone.md");
    std::os::unix::fs::symlink(&gone, fixture.repo().join("AGENTS.md")).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert!(!gone.exists());
    assert!(
        text(&out.stdout).contains("AGENTS.md is a symlink, so init left it alone"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_symlinked_agents_md_with_a_loopherd_section_gets_no_step() {
    let fixture = Fixture::new(&["acme"]);
    let outside = fixture.root.path().join("global.md");
    fs::write(&outside, "## loopherd\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.repo().join("AGENTS.md")).unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "## loopherd\n");
    assert!(
        !text(&out.stdout).contains("AGENTS.md"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_loopherd_heading_on_the_first_line_after_a_bom_is_a_loopherd_section() {
    let fixture = Fixture::new(&["acme"]);
    let agents = "\u{feff}# loopherd\n\nOurs.\n";
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    assert_ok(&fixture.init(&[]));

    assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
}

#[test]
fn on_a_terminal_declining_the_summary_leaves_an_existing_agents_md_as_it_was() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(fixture.repo().join("AGENTS.md"), "# Widgets\n").unwrap();

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\n\n\nn\n");

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains("nothing written"));
    assert_eq!(repo_file(&fixture, "AGENTS.md"), "# Widgets\n");
}

#[test]
fn an_underline_after_a_list_item_or_blockquote_is_not_a_loopherd_heading() {
    for lines in [
        "- Use loopherd to run the herd\n---\n",
        "> Note: loopherd runs here\n---\n",
        "- Use loopherd\n===\n",
        "1. Use\nloopherd\n---\n",
    ] {
        let fixture = Fixture::new(&["acme"]);
        let agents = format!("# Widgets\n\n## Tools\n\n{lines}\n## Style\n");
        fs::write(fixture.repo().join("AGENTS.md"), &agents).unwrap();

        assert_ok(&fixture.init(&[]));

        let written = repo_file(&fixture, "AGENTS.md");
        assert!(
            written.starts_with(&agents) && written.contains("\n## loopherd\n"),
            "{lines:?}: {written}"
        );
    }
}

#[test]
fn a_backtick_line_with_a_backtick_after_the_run_opens_no_fence() {
    let fixture = Fixture::new(&["acme"]);
    let agents = "# Widgets\n\n```js`x\n\n## loopherd\n\nOurs.\n";
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    assert_ok(&fixture.init(&[]));

    assert_eq!(repo_file(&fixture, "AGENTS.md"), agents);
}

#[test]
fn a_new_repo_doc_init_cant_write_is_refused_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    let agents_dir = fixture.repo().join("docs/agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();
    fs::set_permissions(&agents_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let out = fixture.init(&[]);

    fs::set_permissions(&agents_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(&format!(
            "write {}",
            agents_dir.join("loopherd.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.gh_calls().contains("label create"));
    assert!(!fixture.herds().exists());
}

#[test]
fn a_read_only_repo_without_agents_md_is_refused_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    let agents_dir = fixture.repo().join("docs/agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();
    fs::write(agents_dir.join("loopherd.md"), "ours\n").unwrap();
    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o555)).unwrap();

    let out = fixture.init(&[]);

    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(&format!(
            "write {}",
            fixture.repo().join("AGENTS.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.gh_calls().contains("label create"));
    assert!(!fixture.herds().exists());
}

#[test]
fn an_unwritable_herds_dir_or_config_dir_is_refused_before_any_change() {
    for read_only in ["config", "herds"] {
        let fixture = Fixture::new(&["acme"]);
        let dir = match read_only {
            "herds" => fixture.herds(),
            _ => fixture.config(),
        };
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        let out = fixture.init(&[]);

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(out.status.code(), Some(1), "{read_only}");
        assert!(
            text(&out.stderr).contains(&format!(
                "write {}",
                fixture.herds().join("widgets.toml").display()
            )),
            "{read_only}: {}",
            text(&out.stderr)
        );
        assert!(!fixture.gh_calls().contains("label create"), "{read_only}");
        assert!(!fixture.repo().join("AGENTS.md").exists(), "{read_only}");
        assert!(!fixture.repo().join("docs").exists(), "{read_only}");
    }
}

#[test]
fn a_read_only_repo_root_is_refused_after_a_writable_new_doc() {
    let fixture = Fixture::new(&["acme"]);
    let agents_dir = fixture.repo().join("docs/agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();
    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o555)).unwrap();

    let out = fixture.init(&[]);

    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains(&format!(
            "write {}",
            fixture.repo().join("AGENTS.md").display()
        )),
        "{}",
        text(&out.stderr)
    );
    assert!(!agents_dir.join("loopherd.md").exists());
    assert!(!fixture.gh_calls().contains("label create"));
}

#[test]
fn a_setext_loopherd_heading_after_a_list_or_blockquote_ends_is_a_loopherd_section() {
    for agents in [
        "# Widgets\n\n- Build with cargo\n***\nLoopherd\n========\n\nOurs.\n",
        "- foo\n---\nLoopherd\n---\n",
        "> quote\n---\nLoopherd\n---\n",
        "- - -\nLoopherd\n---\n",
        "* * *\nLoopherd\n===\n",
        "Loopherd notes\n2. step\n---\n",
        "Loopherd\n10. x\n---\n",
        ">\nLoopherd\n---\n",
        "1.\nLoopherd\n---\n",
        "- one\n\nLoopherd\n========\n",
        "- one\n# h\nLoopherd\n---\n",
        "- one\n```\n```\nLoopherd\n---\n",
        "*\nLoopherd\n---\n",
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

        assert_ok(&fixture.init(&[]));

        assert_eq!(repo_file(&fixture, "AGENTS.md"), agents, "{agents:?}");
    }
}

#[test]
fn a_tilde_line_with_a_backtick_after_the_run_opens_a_fence() {
    let fixture = Fixture::new(&["acme"]);
    let agents = "# Widgets\n\n~~~js`x\n## loopherd\n~~~\n";
    fs::write(fixture.repo().join("AGENTS.md"), agents).unwrap();

    assert_ok(&fixture.init(&[]));

    let written = repo_file(&fixture, "AGENTS.md");
    assert!(
        written.starts_with(agents) && written.contains("~~~\n\n## loopherd\n"),
        "{written}"
    );
}

#[test]
fn a_relative_config_dir_in_a_cwd_init_cant_write_is_refused_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    let agents_dir = fixture.repo().join("docs/agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();
    fs::write(fixture.repo().join("AGENTS.md"), "# W\n\n## loopherd\n").unwrap();
    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o555)).unwrap();

    let out = fixture.run(
        Command::new("sh")
            .args([
                "-c",
                r#"LOOPHERD_CONFIG_DIR=cfg exec "$@""#,
                "sh",
                env!("CARGO_BIN_EXE_loopherd"),
                "init",
            ])
            .args(Fixture::CHECKS),
    );

    fs::set_permissions(fixture.repo(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("write cfg/herds/widgets.toml"),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.gh_calls().contains("label create"));
    assert!(!agents_dir.join("loopherd.md").exists());
}

/// Issue files in mattpocock-skills' local-markdown layout, as `(path, text)`.
const LOCAL_ISSUES: &[(&str, &str)] = &[
    (
        ".scratch/auth/spec.md",
        "# Auth\n\nStatus: ready-for-agent\n",
    ),
    (
        ".scratch/auth/issues/01-login-form.md",
        "# 01: Login form\n\nStatus: ready-for-agent\n",
    ),
    (
        ".scratch/auth/issues/02-logout.md",
        "# 02: Logout\n\n**Blocked by:** 01\n\n**Status:** ready-for-agent\n",
    ),
    (
        ".scratch/auth/issues/03-session-expiry.md",
        "# 03: Session expiry\n\nStatus: needs-info\n",
    ),
    (
        ".scratch/auth/issues/04-reset-password.md",
        "# 04: Reset password\n\nStatus: resolved\n\n## Comments\n\nStatus: ready-for-agent\n",
    ),
    (
        ".scratch/auth/issues/05-remember-me.md",
        "# 05: Remember me\n\nBlocked by: 04\nStatus: ready-for-agent\n",
    ),
    (
        ".scratch/auth/issues/06-oauth.md",
        "# 06: OAuth\n\nStatus: ready-for-agent, blocked\n",
    ),
    (
        ".scratch/auth/issues/07-two-factor.md",
        "# 07: Two factor\n\n**Blocked by:** None (can start immediately)\n\n\
         **Status:** Ready-for-agent, bug, skill:diagnosing-bugs\n",
    ),
    (
        ".scratch/auth/issues/08-pick-a-provider.md",
        "# 08: Pick a provider\n\nStatus: ready-for-agent ready-for-human\n",
    ),
    (
        ".scratch/auth/issues/09-audit-log.md",
        "# 09: Audit log\n\nBlocked by: the login form\nStatus: ready-for-agent\n",
    ),
    (
        ".scratch/billing/issues/01-invoices.md",
        "# 01: Invoices\n\nStatus: ready-for-agent, documentation\n",
    ),
];

impl Fixture {
    /// Commits `issues` and pushes them as `main` of a bare repo that stands
    /// in for origin, returning the env that sends origin's URL there.
    fn push_local_issues(&self, issues: &[(&str, &str)]) -> Vec<(String, String)> {
        let origin = self.root.path().join("origin.git");
        git(
            self.root.path(),
            &["init", "-q", "--bare", origin.to_str().unwrap()],
        );
        for (path, text) in issues {
            let path = self.repo().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        git(&self.repo(), &["add", ".scratch"]);
        git(
            &self.repo(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "issues",
            ],
        );
        git(
            &self.repo(),
            &["push", "-q", origin.to_str().unwrap(), "HEAD:main"],
        );
        vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                format!("url.{}.insteadOf", origin.display()),
            ),
            (
                "GIT_CONFIG_VALUE_0".into(),
                "git@github.com:acme/widgets.git".into(),
            ),
        ]
    }

    fn loopherd_with(&self, env: &[(String, String)], args: &[&str]) -> Output {
        self.run(
            Command::new(env!("CARGO_BIN_EXE_loopherd"))
                .args(args)
                .envs(env.iter().map(|(k, v)| (k, v))),
        )
    }

    fn herd_get(&self, key: &str) -> String {
        let out = self.loopherd(&["herd", "get", "widgets", key]);
        assert_ok(&out);
        toml::from_str::<toml::Table>(&format!("v = {}", text(&out.stdout))).unwrap()["v"]
            .as_str()
            .unwrap()
            .into()
    }
}

#[test]
fn a_local_tracker_herd_feeds_the_ready_unblocked_issues_pushed_to_the_default_branch() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(LOCAL_ISSUES);
    let draft = fixture.repo().join(".scratch/billing/issues/02-draft.md");
    fs::write(draft, "# 02: Draft\n\nStatus: ready-for-agent\n").unwrap();

    assert_ok(&fixture.init(&["--tracker", "local", "--max", "4"]));

    assert_ok(&fixture.loopherd(&["herds"]));
    let feed = fixture.loopherd_with(&env, &["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert_eq!(
        text(&feed.stdout),
        "source: skipped .scratch/auth/issues/09-audit-log.md: its Blocked by item \
         \"the login form\" names no issue number\n\
         would add widgets-auth-01 (template issue)\n\
         would add widgets-auth-05 (template issue)\n\
         would add widgets-auth-07 (template issue)\n\
         would add widgets-billing-01 (template issue)\n\
         widgets: 0 would retire, 4 would add, 0 skipped, 0 errors\n"
    );
}

#[test]
fn the_local_queue_names_each_issues_id_type_skill_slug_and_path() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(LOCAL_ISSUES);
    assert_ok(&fixture.init(&["--tracker", "local"]));

    let tasks = fixture.run(
        Command::new("sh")
            .args(["-c", &fixture.herd_get("queue.source")])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );

    assert_ok(&tasks);
    assert_eq!(
        text(&tasks.stdout),
        "auth-01 feat tdd login-form .scratch/auth/issues/01-login-form.md\n\
         auth-05 feat tdd remember-me .scratch/auth/issues/05-remember-me.md\n\
         auth-07 fix diagnosing-bugs two-factor .scratch/auth/issues/07-two-factor.md\n\
         billing-01 docs tdd invoices .scratch/billing/issues/01-invoices.md\n"
    );
    assert_eq!(fixture.herd_get("template.issue.name"), "{herd}-{id}");
    assert_eq!(
        fixture.herd_get("template.issue.branch"),
        "{type}/{id}-{slug}"
    );
}

#[test]
fn a_long_feature_name_is_cut_and_hashed_so_the_member_name_fits() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(&[(
        ".scratch/a-very-long-feature-name-indeed/issues/03-first.md",
        "Status: ready-for-agent\n",
    )]);
    assert_ok(&fixture.init(&["--tracker", "local"]));

    let feed = fixture.loopherd_with(&env, &["feed", "widgets", "--dry-run"]);

    assert_ok(&feed);
    let name = "widgets-a-very-long-featu_ha2-03";
    assert_eq!(name.len(), 32);
    assert!(
        text(&feed.stdout).starts_with(&format!("would add {name} (template issue)\n")),
        "{}",
        text(&feed.stdout)
    );
}

#[test]
fn two_cut_features_whose_names_hash_alike_still_get_different_ids() {
    let fixture = Fixture::new(&["acme"]);

    let tasks = fixture.local_tasks(&[
        (
            ".scratch/a-very-long-feature-name-000/issues/01-first.md",
            "Status: ready-for-agent\n",
        ),
        (
            ".scratch/a-very-long-feature-name-aqo/issues/01-second.md",
            "Status: ready-for-agent\n",
        ),
    ]);

    let ids: Vec<&str> = tasks
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{tasks}");
    assert_eq!(ids[0], "a-very-long-featu_eno-01", "{tasks}");
    assert!(
        ids[1].starts_with("a-very-long-featu_") && ids[1] != ids[0],
        "{tasks}"
    );
}

#[test]
fn the_local_done_check_passes_once_the_default_branch_marks_the_issue_resolved() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(LOCAL_ISSUES);
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let done = fixture.herd_get("template.issue.done");

    let check = |path: &str| {
        fixture
            .run(
                Command::new("sh")
                    .args([
                        "-c",
                        &done
                            .replace("{path}", path)
                            .replace("{{", "{")
                            .replace("}}", "}"),
                    ])
                    .envs(env.iter().map(|(k, v)| (k, v))),
            )
            .status
            .code()
    };

    assert_eq!(check(".scratch/auth/issues/04-reset-password.md"), Some(0));
    assert_eq!(check(".scratch/auth/issues/01-login-form.md"), Some(1));
    assert_eq!(check(".scratch/auth/issues/03-session-expiry.md"), Some(1));
}

#[test]
fn a_local_tracker_with_a_github_origin_keeps_the_pr_merged_check_and_creates_no_labels() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--tracker", "local"]);

    assert_ok(&out);
    assert_eq!(
        fixture.herd_get("template.issue.merged"),
        "GH_TOKEN=$(gh auth token --user acme) gh pr list --repo acme/widgets --head {branch} \
         --state merged --json number --jq 'length > 0' | grep -qx true"
    );
    assert!(
        !fixture.gh_calls().contains("label"),
        "{}",
        fixture.gh_calls()
    );
    assert!(
        !text(&out.stdout).contains("placeholder") && !text(&out.stdout).contains("Fill [queue]"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn the_local_worker_prompt_claims_and_resolves_by_editing_the_issue_file() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--tracker", "local"]));

    let prompt = fixture.herd_get("template.issue.prompt");

    for expected in [
        "{path}",
        "`Status:` line to `claimed`",
        "claimed by {name}",
        "## Comments",
        "`Status:` line to `resolved`",
        "`Status:` line to `needs-info`",
        "the {skill} skill",
        "cargo test",
        "GH_TOKEN=$(gh auth token --user acme) gh pr create --repo acme/widgets --base main",
        "loopherd report --kind done",
    ] {
        assert!(
            prompt.contains(expected),
            "missing {expected:?} in\n{prompt}"
        );
    }
    for unexpected in ["gh issue", "Closes #"] {
        assert!(!prompt.contains(unexpected), "{unexpected:?} in\n{prompt}");
    }
    let doc = repo_file(&fixture, "docs/agents/loopherd.md");
    assert!(
        doc.contains("`Status:`") && !doc.contains("Closes #"),
        "{doc}"
    );
    let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
    assert!(
        lead.contains(".scratch/") && !lead.contains("Closes #"),
        "{lead}"
    );
}

#[test]
fn an_existing_local_markdown_tracker_doc_gets_the_local_snippets() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(LOCAL_ISSUES);
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(
        fixture.repo().join("docs/agents/issue-tracker.md"),
        "# Issue Tracker: local markdown\n\nOur words.\n",
    )
    .unwrap();

    assert_ok(&fixture.init(&["--max", "4"]));

    let feed = fixture.loopherd_with(&env, &["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert!(
        text(&feed.stdout).contains("4 would add"),
        "{}",
        text(&feed.stdout)
    );
}

impl Fixture {
    /// Swaps the fake gh for one that fails on any call, logging it.
    fn fail_gh(&self) {
        fs::write(
            self.gh_dir().join("gh"),
            "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls\"\nexit 1\n",
        )
        .unwrap();
    }

    /// Pushes `issues` to a bare repo and makes it origin, with origin/HEAD at main.
    fn push_local_issues_to_plain_origin(&self, issues: &[(&str, &str)]) {
        self.push_local_issues(issues);
        let origin = self.root.path().join("origin.git");
        git(
            &self.repo(),
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        );
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&self.repo(), &["fetch", "-q", "origin"]);
        git(&self.repo(), &["remote", "set-head", "origin", "main"]);
    }
}

#[test]
fn a_local_tracker_without_github_makes_no_gh_call_and_feeds_from_origin() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    let out = fixture.init(&["--tracker", "local", "--max", "4"]);

    assert_ok(&out);
    assert_eq!(fixture.gh_calls(), "");
    assert!(
        text(&out.stderr).contains("warning: without GitHub the herd has no merged check"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        !herd_toml(&fixture)["template"]["issue"]
            .as_table()
            .unwrap()
            .contains_key("merged")
    );
    let feed = fixture.loopherd(&["feed", "widgets", "--dry-run"]);
    assert_ok(&feed);
    assert!(
        text(&feed.stdout).contains("4 would add"),
        "{}",
        text(&feed.stdout)
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn a_local_tracker_without_github_hands_in_a_pushed_branch_and_never_mentions_gh() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    assert_ok(&fixture.init(&["--tracker", "local"]));

    let prompt = fixture.herd_get("template.issue.prompt");
    assert!(prompt.contains("`git push -u origin HEAD`"), "{prompt}");
    assert!(
        prompt.contains("the human merges the branch into main"),
        "{prompt}"
    );
    assert!(
        prompt.contains("\"<branch and one-line summary>\""),
        "{prompt}"
    );
    let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
    let doc = repo_file(&fixture, "docs/agents/loopherd.md");
    for (what, text) in [("prompt", &prompt), ("lead", &lead), ("doc", &doc)] {
        for unexpected in [" gh ", "`gh ", "GH_TOKEN", "acme", "PR"] {
            assert!(
                !text.contains(unexpected),
                "{unexpected:?} in the {what}:\n{text}"
            );
        }
    }
    assert!(doc.contains("open no pull request"), "{doc}");
    assert!(!lead.contains("On `<member> merged`"), "{lead}");
    assert!(
        lead.contains("No `<member> merged` line comes, as the herd\n   has no merged check"),
        "{lead}"
    );
}

impl Fixture {
    /// Commits `issues` and sets origin/main to them under a GitHub origin,
    /// returning env under which origin can't be reached, so init falls back
    /// to the local origin/HEAD.
    fn commit_local_issues_under_github_origin(
        &self,
        issues: &[(&str, &str)],
    ) -> Vec<(String, String)> {
        self.push_local_issues(issues);
        git(
            &self.repo(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(
            &self.repo(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        vec![("GIT_SSH_COMMAND".into(), "false".into())]
    }

    /// Asserts init made no gh call, warned that `shown` has no merged
    /// check, and wrote a herd with no merged check and no PR hand-in.
    fn assert_herd_without_github(&self, out: &Output, shown: &[u8], warning: &str) {
        assert_ok(out);
        assert_eq!(self.gh_calls(), "");
        assert!(text(shown).contains(warning), "{}", text(shown));
        assert!(
            !herd_toml(self)["template"]["issue"]
                .as_table()
                .unwrap()
                .contains_key("merged")
        );
        let prompt = self.herd_get("template.issue.prompt");
        assert!(prompt.contains("`git push -u origin HEAD`"), "{prompt}");
        assert!(!prompt.contains("GH_TOKEN"), "{prompt}");
    }
}

#[test]
fn no_github_gives_a_github_origin_a_local_herd_without_gh() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.commit_local_issues_under_github_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    let mut args = vec!["init", "--tracker", "local", "--no-github"];
    args.extend(Fixture::CHECKS);
    let out = fixture.loopherd_with(&env, &args);

    fixture.assert_herd_without_github(
        &out,
        &out.stderr,
        "warning: without GitHub the herd has no merged check",
    );
    assert!(
        !text(&out.stderr).contains("--repo"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn diff_reads_no_github_back_from_a_herd_init_wrote_without_it() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.commit_local_issues_under_github_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    let mut args = vec!["init", "--tracker", "local", "--no-github"];
    args.extend(Fixture::CHECKS);
    assert_ok(&fixture.loopherd_with(&env, &args));

    let out = fixture.loopherd_with(&env, &["init", "--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn on_a_terminal_a_gh_account_keeps_a_github_origins_local_herd_on_github() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.on_terminal(
        &[
            "--check",
            "cargo test",
            "--tracker",
            "local",
            "--gh-account",
            "acme",
        ],
        "\n\n\n\n\n\n\n\n\n\n",
    );

    assert_ok(&out);
    assert!(
        fixture
            .herd_get("template.issue.merged")
            .contains("--repo acme/widgets")
    );
}

#[test]
fn on_a_terminal_a_github_origins_local_herd_takes_enter_for_no_github() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.commit_local_issues_under_github_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    let envs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let out = fixture.on_terminal_with(
        &envs,
        &["--check", "cargo test", "--tracker", "local"],
        "\n\n\n\n\n\n\n\n\n\n",
    );

    assert!(
        text(&out.stdout)
            .contains("GitHub repo (owner/name, origin is acme/widgets); Enter for none:"),
        "{}",
        text(&out.stdout)
    );
    fixture.assert_herd_without_github(&out, &out.stdout, "Without GitHub it has no merged check");
}

#[test]
fn no_github_refuses_another_tracker_a_repo_and_an_account() {
    let fixture = Fixture::new(&["acme"]);

    for (args, refusal) in [
        (
            &["--tracker", "github", "--no-github"][..],
            "--no-github goes with --tracker local",
        ),
        (
            &["--no-github"][..],
            "--no-github goes with --tracker local",
        ),
        (
            &[
                "--tracker",
                "local",
                "--no-github",
                "--repo",
                "acme/widgets",
            ][..],
            "cannot be used with",
        ),
        (
            &["--tracker", "local", "--no-github", "--gh-account", "acme"][..],
            "cannot be used with",
        ),
    ] {
        let out = fixture.init(args);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out.stderr).contains(refusal), "{}", text(&out.stderr));
    }
    assert_eq!(fixture.gh_calls(), "");

    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(
        fixture.repo().join("docs/agents/issue-tracker.md"),
        "# Issue tracker: GitHub\n",
    )
    .unwrap();
    let out = fixture.init(&["--no-github"]);
    assert!(
        text(&out.stderr).contains(
            "--no-github needs a local tracker, and docs/agents/issue-tracker.md names another"
        ),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_local_tracker_without_github_takes_origins_default_branch_not_the_checked_out_one() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues(LOCAL_ISSUES);
    let origin = fixture.root.path().join("origin.git");
    git(
        &fixture.repo(),
        &["remote", "set-url", "origin", origin.to_str().unwrap()],
    );
    git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&fixture.repo(), &["checkout", "-q", "-b", "feat/x"]);
    git(&fixture.repo(), &["push", "-q", "origin", "HEAD"]);
    git(&fixture.repo(), &["fetch", "-q", "origin"]);
    fixture.fail_gh();

    assert_ok(&fixture.init(&["--tracker", "local"]));

    let source = fixture.herd_get("queue.source");
    assert!(source.contains("git fetch -q origin main"), "{source}");
    assert!(!source.contains("feat/x"), "{source}");
}

#[test]
fn a_local_tracker_without_an_origin_remote_is_refused_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues(LOCAL_ISSUES);
    git(&fixture.repo(), &["remote", "remove", "origin"]);
    fixture.fail_gh();

    assert_refused(
        &fixture,
        &fixture.init(&["--tracker", "local"]),
        &format!(
            "loopherd init: {} has no origin remote; a herd's members branch from origin \
             and push to it, so add one\n",
            fixture.repo().display()
        ),
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn gh_account_without_a_github_repo_is_refused_before_any_change() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    assert_refused(
        &fixture,
        &fixture.init(&["--tracker", "local", "--gh-account", "acme"]),
        "loopherd init: --gh-account needs a GitHub repo; pass --repo <owner>/<name> or drop it\n",
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn a_local_tracker_without_github_still_takes_repo() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);

    assert_ok(&fixture.init(&["--tracker", "local", "--repo", "acme/widgets"]));

    assert!(
        fixture
            .herd_get("template.issue.prompt")
            .contains("gh pr create --repo acme/widgets --base main"),
    );
}

#[test]
fn on_a_terminal_a_local_tracker_without_github_takes_enter_as_none() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    let out = fixture.init_on_terminal(&[], "local\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("GitHub repo (owner/name); Enter for none: "),
        "{shown}"
    );
    assert!(shown.contains("Default branch (main): "), "{shown}");
    assert!(shown.contains("\n  branch  main\r\n"), "{shown}");
    assert!(
        !shown.contains("gh account") && !shown.contains("GitHub  "),
        "{shown}"
    );
    let note = "Without GitHub it has no merged check, so after_merge never runs for its \
                members.\r\nWrite this herd?";
    assert!(shown.contains(note), "{shown}");
    assert!(!shown.contains("warning: without GitHub"), "{shown}");
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn a_default_branch_the_herds_shell_commands_cant_hold_is_refused() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json("repo.json", r#"{"defaultBranchRef": {"name": "a{b}"}}"#);

    let out = fixture.init(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("init takes a branch of letters, digits and - _ . / only"),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().join("widgets.toml").exists());
}

impl Fixture {
    /// The tasks the herd's queue source prints for `issues` pushed to origin.
    fn local_tasks(&self, issues: &[(&str, &str)]) -> String {
        let tasks = self.local_source(issues);
        assert_ok(&tasks);
        text(&tasks.stdout)
    }

    /// Runs the herd's queue source over `issues` pushed to origin.
    fn local_source(&self, issues: &[(&str, &str)]) -> Output {
        let env = self.push_local_issues(issues);
        assert_ok(&self.init(&["--tracker", "local"]));
        self.run(
            Command::new("sh")
                .args(["-c", &self.herd_get("queue.source")])
                .envs(env.iter().map(|(k, v)| (k, v))),
        )
    }
}

#[test]
fn an_issue_without_a_final_newline_or_with_crlf_lines_still_reads() {
    let fixture = Fixture::new(&["acme"]);

    let tasks = fixture.local_tasks(&[
        (".scratch/f/issues/01-a.md", "Status: ready-for-agent"),
        (".scratch/f/issues/02-b.md", "Status: ready-for-agent\n"),
        (
            ".scratch/f/issues/03-c.md",
            "# 03\r\n\r\nStatus: ready-for-agent\r\n",
        ),
    ]);

    assert_eq!(
        tasks,
        "f-01 feat tdd a .scratch/f/issues/01-a.md\n\
         f-02 feat tdd b .scratch/f/issues/02-b.md\n\
         f-03 feat tdd c .scratch/f/issues/03-c.md\n"
    );
}

#[test]
fn the_local_done_check_says_not_done_for_a_missing_file_and_reads_crlf() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(&[(".scratch/f/issues/01-a.md", "Status: resolved\r\n")]);
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let done = fixture.herd_get("template.issue.done");
    let check = |path: &str| {
        let script = done
            .replace("{path}", path)
            .replace("{{", "{")
            .replace("}}", "}");
        fixture
            .run(
                Command::new("sh")
                    .args(["-c", &script])
                    .envs(env.iter().map(|(k, v)| (k, v))),
            )
            .status
            .code()
    };

    assert_eq!(check(".scratch/f/issues/01-a.md"), Some(0));
    assert_eq!(check(".scratch/f/issues/09-gone.md"), Some(1));
}

#[test]
fn blocked_by_blocks_on_every_number_it_names_and_on_a_list_below_it_unless_none() {
    let fixture = Fixture::new(&["acme"]);
    let blocked_by = |n: &str, line: &str| {
        (
            format!(".scratch/f/issues/{n}-x.md"),
            format!("{line}\nStatus: ready-for-agent\n"),
        )
    };
    let issues = [
        (
            ".scratch/f/issues/01-done.md".to_string(),
            "Status: resolved\n".to_string(),
        ),
        (
            ".scratch/f/issues/02-open.md".to_string(),
            "Status: needs-info\n".to_string(),
        ),
        blocked_by("03", "Blocked by: 01 and 02"),
        blocked_by("04", "Blocked by: 01 02"),
        blocked_by("05", "Blocked by: #01 / #02"),
        blocked_by("06", "**Blocked by:**\n\n- 01\n- 02\n"),
        blocked_by("07", "Blocked by: 01: Login form; #01"),
        blocked_by("08", "**Blocked by:**\n- 01\n"),
        blocked_by("09", "Blocked by:\n- None\n"),
    ];
    let issues: Vec<(&str, &str)> = issues
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();

    let tasks = fixture.local_tasks(&issues);

    assert_eq!(
        tasks,
        "f-07 feat tdd x .scratch/f/issues/07-x.md\n\
         f-08 feat tdd x .scratch/f/issues/08-x.md\n\
         f-09 feat tdd x .scratch/f/issues/09-x.md\n"
    );
}

#[test]
fn only_a_word_starting_with_a_number_names_a_blocker_and_a_missing_one_is_noted() {
    let fixture = Fixture::new(&["acme"]);
    let blocked_by = |n: &str, line: &str| {
        (
            format!(".scratch/f/issues/{n}-x.md"),
            format!("{line}\nStatus: ready-for-agent\n"),
        )
    };
    let issues = [
        (
            ".scratch/f/issues/01-step-2-of-3.md".to_string(),
            "Status: resolved\n".to_string(),
        ),
        blocked_by("04", "**Blocked by:** 01 (Node 20 upgrade)"),
        blocked_by("05", "Blocked by: .scratch/f/issues/01-step-2-of-3.md"),
        blocked_by("06", "Blocked by: 01: Upgrade to Node 20"),
        blocked_by("07", "Blocked by: 01, 20"),
    ];
    let issues: Vec<(&str, &str)> = issues
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();

    let out = fixture.local_source(&issues);

    assert_ok(&out);
    assert_eq!(
        text(&out.stdout),
        "f-04 feat tdd x .scratch/f/issues/04-x.md\n\
         f-05 feat tdd x .scratch/f/issues/05-x.md\n\
         f-06 feat tdd x .scratch/f/issues/06-x.md\n"
    );
    assert_eq!(
        text(&out.stderr),
        "skipped .scratch/f/issues/07-x.md: its Blocked by names 20, which is no issue \
         of its feature\n"
    );
}

#[test]
fn a_blocker_names_its_first_numbers_and_a_path_names_its_own_feature() {
    let fixture = Fixture::new(&["acme"]);
    let blocked_by = |n: &str, line: &str| {
        (
            format!(".scratch/f/issues/{n}-x.md"),
            format!("{line}\nStatus: ready-for-agent\n"),
        )
    };
    let issues = [
        (
            ".scratch/f/issues/01-a.md".to_string(),
            "Status: resolved\n".to_string(),
        ),
        (
            ".scratch/f/issues/02-b.md".to_string(),
            "Status: needs-info\n".to_string(),
        ),
        (
            ".scratch/g/issues/02-c.md".to_string(),
            "Status: resolved\n".to_string(),
        ),
        blocked_by("10", "Blocked by: 01 - Node 20 upgrade"),
        blocked_by("11", "Blocked by: #01 Node 20 upgrade"),
        blocked_by("12", "Blocked by: see 01"),
        blocked_by("13", "Blocked by: .scratch/g/issues/02-c.md"),
        blocked_by("14", "Blocked by: Node upgrade (#01)"),
        blocked_by("15", "Blocked by: .scratch/h/issues/01-d.md"),
    ];
    let issues: Vec<(&str, &str)> = issues
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();

    let out = fixture.local_source(&issues);

    assert_ok(&out);
    assert_eq!(
        text(&out.stdout),
        "f-10 feat tdd x .scratch/f/issues/10-x.md\n\
         f-11 feat tdd x .scratch/f/issues/11-x.md\n\
         f-12 feat tdd x .scratch/f/issues/12-x.md\n\
         f-13 feat tdd x .scratch/f/issues/13-x.md\n"
    );
    assert_eq!(
        text(&out.stderr),
        "skipped .scratch/f/issues/14-x.md: its Blocked by item \"node upgrade (#01)\" \
         names no issue number\n\
         skipped .scratch/f/issues/15-x.md: its Blocked by names \
         .scratch/h/issues/01-d.md, which is no issue\n"
    );
}

#[test]
fn each_blocker_path_names_its_own_feature_and_a_relative_one_names_the_issues() {
    let fixture = Fixture::new(&["acme"]);
    let ready = |line: &str| format!("Blocked by: {line}\nStatus: ready-for-agent\n");
    let issues = [
        (
            ".scratch/auth/issues/03-a.md",
            "Status: resolved\n".to_string(),
        ),
        (
            ".scratch/auth/issues/04-node.md",
            "Status: resolved\n".to_string(),
        ),
        (
            ".scratch/billing/issues/03-x.md",
            "Status: needs-info\n".to_string(),
        ),
        (
            ".scratch/auth/issues/10-x.md",
            ready(".scratch/auth/issues/04-node.md and .scratch/billing/issues/03-x.md"),
        ),
        (
            ".scratch/auth/issues/11-x.md",
            ready("../issues/04-node.md"),
        ),
        (".scratch/auth/issues/12-x.md", ready("./issues/04-node.md")),
        (
            ".scratch/auth/issues/13-x.md",
            ready("../../issues/04-node.md"),
        ),
        (
            ".scratch/auth/issues/14-x.md",
            ready(".scratch/auth/../issues/04-node.md"),
        ),
    ];
    let issues: Vec<(&str, &str)> = issues.iter().map(|(p, t)| (*p, t.as_str())).collect();

    let out = fixture.local_source(&issues);

    assert_ok(&out);
    assert_eq!(
        text(&out.stdout),
        "auth-11 feat tdd x .scratch/auth/issues/11-x.md\n\
         auth-12 feat tdd x .scratch/auth/issues/12-x.md\n"
    );
    assert_eq!(
        text(&out.stderr),
        "skipped .scratch/auth/issues/13-x.md: its Blocked by names \
         ../../issues/04-node.md, which is no issue\n\
         skipped .scratch/auth/issues/14-x.md: its Blocked by names \
         .scratch/auth/../issues/04-node.md, which is no issue\n"
    );
}

#[test]
fn an_issue_file_whose_path_the_queue_cant_read_is_noted_on_stderr() {
    let fixture = Fixture::new(&["acme"]);
    let ready = "Status: ready-for-agent\n";

    let out = fixture.local_source(&[
        (".scratch/auth/issues/01-login.md", ready),
        (".scratch/My Feature/issues/01-x.md", ready),
        (".scratch/auth/issues/02_logout.md", ready),
        (".scratch/auth/spec.md", ready),
    ]);

    assert_ok(&out);
    assert_eq!(
        text(&out.stdout),
        "auth-01 feat tdd login .scratch/auth/issues/01-login.md\n"
    );
    let rule = "an issue path is .scratch/<feature>/issues/<NN>-<slug>.md of letters, \
                digits and . _ -";
    assert_eq!(
        text(&out.stderr),
        format!(
            "skipped .scratch/My Feature/issues/01-x.md: {rule}\n\
             skipped .scratch/auth/issues/02_logout.md: {rule}\n"
        )
    );
}

#[test]
fn the_local_source_and_done_retry_a_fetch_that_lost_a_race() {
    let fixture = Fixture::new(&["acme"]);
    let env = fixture.push_local_issues(&[(".scratch/f/issues/01-a.md", "Status: resolved\n")]);
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let lost = fixture.root.path().join("lost");
    let script = fixture.gh_dir().join("git");
    executable(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = fetch ] && [ ! -e {lost} ]; then\n  : > {lost}\n  \
             echo \"error: cannot lock ref 'refs/remotes/origin/main'\" >&2\n  exit 1\nfi\n\
             exec {real_git} \"$@\"\n",
            lost = lost.display(),
            real_git = real_git.trim(),
        ),
    );
    let run = |snippet: String| {
        let _ = fs::remove_file(&lost);
        fixture.run(
            Command::new("sh")
                .args(["-c", &snippet])
                .envs(env.iter().map(|(k, v)| (k, v))),
        )
    };

    let source = run(fixture.herd_get("queue.source"));
    let done = run(fixture
        .herd_get("template.issue.done")
        .replace("{path}", ".scratch/f/issues/01-a.md"));

    assert_ok(&source);
    assert_eq!(text(&source.stderr), "");
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    assert_eq!(text(&done.stderr), "");
    assert!(lost.exists());
}

#[test]
fn features_that_cut_or_reshape_to_the_same_id_get_distinct_ids() {
    let fixture = Fixture::new(&["acme"]);

    let tasks = fixture.local_tasks(&[
        (
            ".scratch/Auth_v2/issues/01-a.md",
            "Status: ready-for-agent\n",
        ),
        (
            ".scratch/auth-v2/issues/01-b.md",
            "Status: ready-for-agent\n",
        ),
        (
            ".scratch/user-settings-page-redesign/issues/01-c.md",
            "Status: ready-for-agent\n",
        ),
        (
            ".scratch/user-settings-page-reorder/issues/01-d.md",
            "Status: ready-for-agent\n",
        ),
    ]);

    let ids: Vec<&str> = tasks
        .lines()
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(ids.len(), 4, "{tasks}");
    assert!(ids.contains(&"auth-v2-01"), "{tasks}");
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 4, "{tasks}");
    assert!(
        ids.iter().all(|id| "widgets-".len() + id.len() <= 32),
        "{tasks}"
    );
}

/// The herd's files and the repo's, so `--diff` can be shown to write nothing.
fn everything(fixture: &Fixture) -> Vec<(PathBuf, String)> {
    let mut out = fixture.config_listing();
    let mut dirs = vec![fixture.repo()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true if path.ends_with(".git") => {}
                true => dirs.push(path),
                false => out.push((path.clone(), fs::read_to_string(&path).unwrap())),
            }
        }
    }
    out.sort();
    out
}

#[test]
fn diff_right_after_init_prints_nothing_writes_nothing_and_exits_0() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let before = everything(&fixture);
    fs::remove_file(fixture.gh_dir().join("calls")).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(everything(&fixture), before);
    assert!(
        !fixture.gh_calls().contains("label create"),
        "{}",
        fixture.gh_calls()
    );
}

#[test]
fn diff_shows_an_edited_herd_file_against_todays_built_in_and_exits_1() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let herd_file = fixture.herds().join("widgets.toml");
    let edited = fs::read_to_string(&herd_file)
        .unwrap()
        .replace("args = [\"--permission-mode\", \"auto\"]\n", "args = []\n");
    fs::write(&herd_file, &edited).unwrap();
    let before = everything(&fixture);

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let shown = herd_file.display();
    assert!(
        text(&out.stdout).starts_with(&format!("--- {shown}\n+++ {shown} (init now)\n@@ ")),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stdout).contains("\n-args = []\n+args = [\"--permission-mode\", \"auto\"]\n"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(text(&out.stdout).matches("\n--- ").count(), 0);
    assert_eq!(everything(&fixture), before);
}

#[test]
fn diff_shows_a_missing_lead_prompt_and_repo_doc_as_whole_new_files() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let lead_file = fixture.herds().join("widgets-lead.md");
    let lead = fs::read_to_string(&lead_file).unwrap();
    fs::remove_file(&lead_file).unwrap();
    let loopherd_doc = fixture.repo().join("docs/agents/loopherd.md");
    fs::remove_file(&loopherd_doc).unwrap();
    let before = everything(&fixture);

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let added: String = lead.lines().map(|line| format!("+{line}\n")).collect();
    assert!(
        stdout.contains(&format!(
            "--- /dev/null\n+++ {} (init now)\n@@ -0,0 +1,{} @@\n{added}",
            lead_file.display(),
            lead.lines().count()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "--- /dev/null\n+++ {} (init now)\n",
            loopherd_doc.display()
        )),
        "{stdout}"
    );
    assert_eq!(everything(&fixture), before);
}

#[test]
fn diff_shows_a_missing_grill_prompt_as_a_whole_new_file() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let grill_file = fixture.herds().join("widgets-grill.md");
    let grill = fs::read_to_string(&grill_file).unwrap();
    fs::remove_file(&grill_file).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let added: String = grill.lines().map(|line| format!("+{line}\n")).collect();
    assert_eq!(
        text(&out.stdout),
        format!(
            "--- /dev/null\n+++ {} (init now)\n@@ -0,0 +1,{} @@\n{added}",
            grill_file.display(),
            grill.lines().count()
        )
    );
}

#[test]
fn diff_shows_drift_in_a_repo_doc_init_wrote() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let tracker = fixture.repo().join("docs/agents/issue-tracker.md");
    let mine = format!("{}Ours too.\n", fs::read_to_string(&tracker).unwrap());
    fs::write(&tracker, mine).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with(&format!(
            "--- {shown}\n+++ {shown} (init now)\n",
            shown = tracker.display()
        )),
        "{stdout}"
    );
    assert!(stdout.ends_with("-Ours too.\n"), "{stdout}");
}

#[test]
fn diff_shows_a_missing_herd_file_as_init_would_write_it_now() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let herd_file = fixture.herds().join("widgets.toml");
    let herd = fs::read_to_string(&herd_file).unwrap();
    fs::remove_file(&herd_file).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let added: String = herd.lines().map(|line| format!("+{line}\n")).collect();
    assert_eq!(
        text(&out.stdout),
        format!(
            "--- /dev/null\n+++ {} (init now)\n@@ -0,0 +1,{} @@\n{added}",
            herd_file.display(),
            herd.lines().count()
        )
    );
    assert!(!herd_file.exists());
}

#[test]
fn diff_infers_no_lead_from_a_herd_file_without_one() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--no-lead"]));

    let out = fixture.init(&["--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_names_the_herd_running_the_repo_whatever_its_name() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--herd", "gadgets"]));

    let out = fixture.init(&["--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_refuses_a_herd_name_that_points_at_another_repo() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.herds()).unwrap();
    fs::write(
        fixture.herds().join("widgets.toml"),
        "repo = \"/elsewhere/widgets\"\n",
    )
    .unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: herd widgets already points at /elsewhere/widgets; \
         pass --herd <name> to name this repo's herd\n"
    );
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_exits_2_when_it_cant_read_a_herd_file() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let herd_file = fixture.herds().join("widgets.toml");
    fs::set_permissions(&herd_file, fs::Permissions::from_mode(0o000)).unwrap();

    let out = fixture.init(&["--diff"]);

    fs::set_permissions(&herd_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).starts_with("loopherd init: can't tell which repo"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_shows_the_loopherd_section_init_would_append_to_a_read_only_agents_md() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let agents_md = fixture.repo().join("AGENTS.md");
    fs::write(&agents_md, "# Mine\n").unwrap();
    fs::set_permissions(&agents_md, fs::Permissions::from_mode(0o444)).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let hunk = format!(
        "--- {shown}\n+++ {shown} (init now)\n@@ -1 +1,",
        shown = agents_md.display()
    );
    let at = stdout.find(&hunk).unwrap_or_else(|| panic!("{stdout}"));
    let body = &stdout[at + hunk.len()..];
    let body = &body[body.find(" @@\n").unwrap() + 4..];
    assert!(body.starts_with(" # Mine\n+\n+## "), "{stdout}");
    assert!(!body.contains("\n-"), "{stdout}");
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), "# Mine\n");
}

#[test]
fn diff_reads_max_context_limit_and_checks_back_from_the_herd_file() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[
        "--max",
        "5",
        "--context-limit",
        "50000",
        "--check",
        "echo {x}",
    ]));

    let out = fixture.loopherd(&["init", "--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_shows_drift_in_loopherd_md_and_triage_labels_md() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));
    let docs = ["docs/agents/loopherd.md", "docs/agents/triage-labels.md"];
    for doc in docs {
        let path = fixture.repo().join(doc);
        let mine = format!("{}Ours.\n", fs::read_to_string(&path).unwrap());
        fs::write(&path, mine).unwrap();
    }

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    for doc in docs {
        let shown = fixture.repo().join(doc).display().to_string();
        let header = format!("--- {shown}\n+++ {shown} (init now)\n");
        let at = stdout
            .find(&header)
            .unwrap_or_else(|| panic!("{doc}: {stdout}"));
        let rest = &stdout[at + header.len()..];
        let end = rest.find("\n--- ").map_or(rest.len(), |end| end + 1);
        assert!(rest[..end].ends_with("-Ours.\n"), "{doc}: {stdout}");
    }
}

#[test]
fn diff_exits_2_on_a_tracker_desc_without_tracker_other() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let out = fixture.init(&["--diff", "--tracker-desc", "x"]);

    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stderr),
        "loopherd init: --tracker-desc goes with --tracker other\n"
    );
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_exits_2_without_home() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let out = fixture.run(
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .args(["init", "--diff"])
            .env_remove("HOME"),
    );

    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("HOME is not set"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn diff_keeps_the_recorded_checks_around_a_reworded_one() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--check", "make lint"]));
    let herd_file = fixture.herds().join("widgets.toml");
    let reworded = fs::read_to_string(&herd_file).unwrap().replace(
        "   - `cargo fmt --check`\n",
        "   - `cargo fmt --check` (strict)\n",
    );
    fs::write(&herd_file, reworded).unwrap();

    let out = fixture.loopherd(&["init", "--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let shown = herd_file.display();
    assert!(
        stdout.starts_with(&format!("--- {shown}\n+++ {shown} (init now)\n")),
        "{stdout}"
    );
    let changed: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with(['-', '+']) && !line.starts_with("--- "))
        .filter(|line| !line.starts_with("+++ "))
        .collect();
    assert_eq!(
        changed,
        [
            "-   - `cargo fmt --check` (strict)",
            "-- `cargo fmt --check`"
        ],
        "{stdout}"
    );
}

#[test]
fn diff_right_after_a_local_init_without_github_prints_nothing_and_makes_no_gh_call() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let before = everything(&fixture);

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), "");
    assert_eq!(everything(&fixture), before);
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn diff_reads_back_the_repo_passed_to_init_over_an_origin_that_isnt_github() {
    for (tracker, guard) in [
        ("local", "hook"),
        ("github", "hook"),
        ("github", "credentials"),
    ] {
        let fixture = Fixture::new(&["acme"]);
        fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
        let init = fixture.init(&[
            "--tracker",
            tracker,
            "--repo",
            "acme/widgets",
            "--merge-guard",
            guard,
        ]);
        assert_ok(&init);
        let before = everything(&fixture);

        let out = fixture.init(&["--diff"]);

        assert_eq!(
            out.status.code(),
            Some(0),
            "{tracker}: {}",
            text(&out.stderr)
        );
        assert_eq!(text(&out.stdout), "", "{tracker}");
        assert_eq!(everything(&fixture), before, "{tracker}");
    }
}

#[test]
fn diff_reads_back_the_repo_from_a_merged_that_starts_with_gh() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    let init = fixture.init(&["--repo", "acme/widgets", "--merge-guard", "credentials"]);
    assert_ok(&init);
    let herd_file = fixture.herds().join("widgets.toml");
    let bare = fs::read_to_string(&herd_file).unwrap().replacen(
        "merged = \"GH_TOKEN=${{GH_TOKEN:-$(gh auth token --user acme)}} gh pr list",
        "merged = \"gh pr list",
        1,
    );
    fs::write(&herd_file, &bare).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("\n-merged = \"gh pr list --repo acme/widgets "),
        "{stdout}"
    );
    assert_eq!(stdout.matches("\n-").count(), 1, "{stdout}");
}

#[test]
fn diff_reads_no_repo_back_from_a_merged_that_isnt_the_gh_check() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let herd_file = fixture.herds().join("widgets.toml");
    let merged = "merged = \"glab mr list --repo group/widgets --source-branch {branch} --merged | grep -q .\"";
    let tailored = fs::read_to_string(&herd_file)
        .unwrap()
        .replace("\nprompt = ", &format!("\n{merged}\nprompt = "));
    fs::write(&herd_file, tailored).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("-{merged}\n")),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn diff_shows_drift_in_each_file_a_local_init_wrote() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    assert_ok(&fixture.init(&["--tracker", "local"]));
    let files = [
        fixture.herds().join("widgets.toml"),
        fixture.herds().join("widgets-lead.md"),
        fixture.repo().join("docs/agents/issue-tracker.md"),
        fixture.repo().join("docs/agents/loopherd.md"),
        fixture.repo().join("docs/agents/triage-labels.md"),
    ];
    for path in &files {
        let mine = format!("{}# Ours.\n", fs::read_to_string(path).unwrap());
        fs::write(path, mine).unwrap();
    }

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    for path in &files {
        let shown = path.display();
        let header = format!("--- {shown}\n+++ {shown} (init now)\n");
        let at = stdout
            .find(&header)
            .unwrap_or_else(|| panic!("{shown}: {stdout}"));
        let rest = &stdout[at + header.len()..];
        let end = rest.find("\n--- ").map_or(rest.len(), |end| end + 1);
        assert!(rest[..end].ends_with("-# Ours.\n"), "{shown}: {stdout}");
    }
    assert_eq!(
        stdout.matches("\n--- ").count() + 1,
        files.len(),
        "{stdout}"
    );
    assert_eq!(fixture.gh_calls(), "");
}

impl Fixture {
    /// A commit on `main`, checked out, so the repo has a current branch.
    fn commit_on(&self, branch: &str) {
        git(&self.repo(), &["checkout", "-q", "-b", branch]);
        git(
            &self.repo(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
    }

    fn set_origin(&self, url: &str) {
        git(&self.repo(), &["remote", "set-url", "origin", url]);
    }
}

const OTHER: [&str; 4] = [
    "--tracker",
    "other",
    "--tracker-desc",
    "Issues live in Linear.",
];

#[test]
fn an_other_tracker_without_an_origin_makes_no_gh_call_and_takes_the_current_branch() {
    let fixture = Fixture::new(&["acme"]);
    git(&fixture.repo(), &["remote", "remove", "origin"]);
    fixture.commit_on("develop");
    fixture.fail_gh();

    let out = fixture.init(&OTHER);

    assert_ok(&out);
    assert_eq!(fixture.gh_calls(), "");
    assert!(
        fixture
            .herd_get("template.issue.prompt")
            .contains("merges it into develop"),
        "{}",
        fixture.herd_get("template.issue.prompt")
    );
}

#[test]
fn an_other_tracker_with_a_gitlab_origin_takes_origin_head_and_omits_merged_with_a_warning() {
    let fixture = Fixture::new(&["acme"]);
    fixture.commit_on("feat/x");
    fixture.set_origin("git@gitlab.com:acme/widgets.git");
    git(
        &fixture.repo(),
        &["update-ref", "refs/remotes/origin/trunk", "HEAD"],
    );
    git(
        &fixture.repo(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    fixture.fail_gh();

    let out = fixture.init(&OTHER);

    assert_ok(&out);
    assert_eq!(fixture.gh_calls(), "");
    assert!(
        text(&out.stderr).contains("warning: without GitHub the herd has no merged check"),
        "{}",
        text(&out.stderr)
    );
    let issue = herd_toml(&fixture)["template"]["issue"].clone();
    assert!(!issue.as_table().unwrap().contains_key("merged"), "{issue}");
    let prompt = fixture.herd_get("template.issue.prompt");
    assert!(prompt.contains("merges it into trunk"), "{prompt}");
    assert!(!prompt.contains("feat/x"), "{prompt}");
}

#[test]
fn an_other_tracker_with_a_github_origin_keeps_the_github_merged_check() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&OTHER));

    assert!(
        fixture
            .herd_get("template.issue.merged")
            .contains("gh pr list --repo acme/widgets --head {branch} --state merged"),
    );
}

#[test]
fn an_other_tracker_names_no_github_issue_and_marks_what_to_fill() {
    for github in [true, false] {
        let fixture = Fixture::new(&["acme"]);
        if !github {
            git(&fixture.repo(), &["remote", "remove", "origin"]);
            fixture.commit_on("main");
            fixture.fail_gh();
        }

        let out = fixture.init(&OTHER);

        assert_ok(&out);
        let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
        let header: String = herd.lines().take_while(|l| l.starts_with('#')).collect();
        let issue = herd_toml(&fixture)["template"]["issue"].clone();
        let done_comment =
            &herd[herd.find("[template.issue]").unwrap()..herd.find("prompt =").unwrap()];
        let prompt = issue["prompt"].as_str().unwrap().to_string();
        let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
        assert!(!issue.as_table().unwrap().contains_key("done"), "{issue}");
        for (what, text) in [
            ("header", &header),
            (
                "done",
                &done_comment.replace(
                    issue
                        .get("merged")
                        .and_then(|m| m.as_str())
                        .unwrap_or("\u{0}"),
                    "",
                ),
            ),
            ("prompt", &prompt),
            ("lead", &lead),
        ] {
            // A GitHub origin's prompts name the gh account for PR commands.
            let names_gh = github && (what == "prompt" || what == "lead");
            let unexpected: &[&str] = match names_gh {
                true => &["GitHub", "Closes #", "gh issue"],
                false => &["GitHub", "Closes #", " gh ", "`gh ", "GH_TOKEN", "acme"],
            };
            for unexpected in unexpected {
                assert!(
                    !text.contains(unexpected),
                    "{unexpected:?} in the {what} (github origin: {github}):\n{text}"
                );
            }
        }
        for (what, text) in [("done", done_comment), ("prompt", &prompt), ("lead", &lead)] {
            assert!(
                text.contains("Fill in from docs/agents/issue-tracker.md"),
                "no placeholder in the {what}:\n{text}"
            );
        }
        let herds = fixture.loopherd(&["herds"]);
        assert!(
            text(&herds.stdout).contains("source is empty"),
            "{}",
            text(&herds.stdout)
        );
        let stdout = text(&out.stdout);
        let next = &stdout[stdout.find("Next:").unwrap()..];
        let herd_file = fixture.herds().join("widgets.toml");
        let lead_file = fixture.herds().join("widgets-lead.md");
        for step in [
            format!(
                "  2. Fill in from docs/agents/issue-tracker.md, in {}: [queue] source, \
                 [queue] backlog, [template.issue] done, and each prompt step marked (fill in).",
                herd_file.display()
            ),
            format!(
                "  3. Fill in from docs/agents/issue-tracker.md, in {}: the line marked (fill in).",
                lead_file.display()
            ),
            format!(
                "  4. Fill in from docs/agents/issue-tracker.md, in {}: [template.grill] done, \
                 and in {} each line marked (fill in).",
                herd_file.display(),
                fixture.herds().join("widgets-grill.md").display()
            ),
            "  5. Fill in each section of docs/agents/loopherd.md marked (fill in), or adapt the \
                 doc to the same tracker if it was already there."
                .to_string(),
        ] {
            assert!(next.contains(&step), "{step:?} missing from:\n{next}");
        }
    }
}

#[test]
fn a_ready_for_agent_label_the_queue_reads_as_a_type_or_skill_is_refused_before_any_gh_call() {
    for (label, clash) in [
        ("bug", "the queue reads bug as the branch type fix"),
        (
            "Documentation",
            "the queue reads documentation as the branch type docs",
        ),
        (
            "skill:tdd",
            "the queue reads a skill: label as the worker's skill",
        ),
    ] {
        for tracker in [
            None,
            Some("# Issue tracker: Local Markdown\n"),
            Some("# Issue tracker: Linear\n"),
        ] {
            let fixture = Fixture::new(&["acme"]);
            write_docs(
                &fixture,
                tracker,
                &format!("| `ready-for-agent` | `{label}` | x |\n"),
            );

            let before = fs::read_dir(fixture.repo().join("docs/agents"))
                .unwrap()
                .count();
            assert_refused(
                &fixture,
                &fixture.init(&[]),
                &format!(
                    "loopherd init: docs/agents/triage-labels.md maps ready-for-agent to \
                     {label}, but {clash}; pick another label\n"
                ),
            );
            assert_eq!(fixture.gh_calls(), "");
            let after = fs::read_dir(fixture.repo().join("docs/agents"))
                .unwrap()
                .count();
            assert_eq!(before, after);
        }
    }
}

#[test]
fn diff_on_another_branch_reads_the_default_branch_back_when_origin_head_is_unset() {
    let fixture = Fixture::new(&["acme"]);
    git(&fixture.repo(), &["remote", "remove", "origin"]);
    fixture.commit_on("main");
    fixture.fail_gh();
    let out = fixture.init(&OTHER);
    assert_ok(&out);
    assert!(
        text(&out.stderr)
            .contains("origin/HEAD isn't set, so the default branch is the checked-out main"),
        "{}",
        text(&out.stderr)
    );
    git(&fixture.repo(), &["checkout", "-q", "-b", "feature"]);

    let out = fixture.init(&["--diff"]);

    assert_ok(&out);
    assert_eq!(text(&out.stdout), "");
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn an_other_tracker_on_a_detached_head_without_origin_head_names_how_to_set_the_branch() {
    let fixture = Fixture::new(&["acme"]);
    git(&fixture.repo(), &["remote", "remove", "origin"]);
    fixture.commit_on("main");
    git(&fixture.repo(), &["checkout", "-q", "--detach"]);
    fixture.fail_gh();

    let out = fixture.init(&OTHER);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains(
            "origin/HEAD isn't set and HEAD is detached; check out the default branch \
             or run `git remote set-head origin <branch>`"
        ),
        "{}",
        text(&out.stderr)
    );
    assert!(!fixture.herds().exists());
}

#[test]
fn an_other_tracker_names_the_gh_account_for_pr_commands_only_with_a_github_origin() {
    for github in [true, false] {
        let fixture = Fixture::new(&["acme"]);
        if !github {
            git(&fixture.repo(), &["remote", "remove", "origin"]);
            fixture.commit_on("main");
            fixture.fail_gh();
        }

        assert_ok(&fixture.init(&OTHER));

        let prompt = fixture.herd_get("template.issue.prompt");
        let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
        let note = "Run every gh command as acme, prefixed with\n\
                    `GH_TOKEN=$(gh auth token --user acme)`";
        let fill_in = "Link it on task {number}, so the task closes when the PR merges. \
                       (fill in)\n6. Finish";
        for (what, text, expected) in [
            ("prompt", &prompt, note),
            (
                "prompt",
                &prompt,
                "`GH_TOKEN=$(gh auth token --user acme) git -c credential.helper= \
                 -c credential.helper='!gh auth git-credential' push -u origin HEAD`",
            ),
            (
                "prompt",
                &prompt,
                "`GH_TOKEN=$(gh auth token --user acme) gh pr create --repo acme/widgets \
                 --base main --head {branch} --assignee acme",
            ),
            (
                "prompt",
                &prompt,
                &format!("tracker's own commands.\n\n{note}"),
            ),
            ("prompt", &prompt, fill_in),
            (
                "prompt",
                &prompt,
                "5. Push the branch as acme:\n   `GH_TOKEN",
            ),
            ("lead", &lead, note),
            (
                "lead",
                &lead,
                "`! GH_TOKEN=$(gh auth token --user acme) gh pr merge <n> --repo acme/widgets",
            ),
        ] {
            assert_eq!(
                text.contains(expected),
                github,
                "{expected:?} in the {what} (github origin: {github}):\n{text}"
            );
        }
        assert!(
            prompt.contains("each line marked (fill in), with this\ntracker's own commands"),
            "{prompt}"
        );
        if !github {
            for (what, text) in [("prompt", &prompt), ("lead", &lead)] {
                for unexpected in [" gh ", "`gh ", "GH_TOKEN", "acme"] {
                    assert!(
                        !text.contains(unexpected),
                        "{unexpected:?} in the {what}:\n{text}"
                    );
                }
            }
        }
    }
}

#[test]
fn an_other_tracker_asks_for_a_pull_request_only_with_a_github_origin() {
    for github in [true, false] {
        let fixture = Fixture::new(&["acme"]);
        if !github {
            git(&fixture.repo(), &["remote", "remove", "origin"]);
            fixture.commit_on("main");
            fixture.fail_gh();
        }

        let out = fixture.init(&OTHER);

        assert_ok(&out);
        let prompt = fixture.herd_get("template.issue.prompt");
        let doc = fs::read_to_string(fixture.repo().join("docs/agents/loopherd.md")).unwrap();
        let (step, hand_in, report) = match github {
            true => (
                "open a pull request into main",
                "open a PR with `--base main`",
                "<PR link and one-line summary>",
            ),
            false => (
                "open no pull request",
                "open no pull request; the human merges it into main",
                "<branch and one-line summary>",
            ),
        };
        assert!(prompt.contains(step), "{prompt}");
        assert!(prompt.contains(report), "{prompt}");
        assert!(doc.contains(hand_in), "{doc}");
        assert!(
            doc.contains("Fill in from docs/agents/issue-tracker.md"),
            "{doc}"
        );
        for unexpected in ["GitHub", " gh ", "`gh ", "acme", "Closes #"] {
            assert!(!doc.contains(unexpected), "{unexpected:?} in:\n{doc}");
        }
        let add_origin = text(&out.stdout).contains("Add an origin remote");
        assert_eq!(add_origin, !github, "{}", text(&out.stdout));
    }
}

#[test]
fn on_a_terminal_a_detached_head_without_origin_head_asks_for_the_default_branch() {
    let fixture = Fixture::new(&["acme"]);
    git(&fixture.repo(), &["remote", "remove", "origin"]);
    fixture.commit_on("main");
    git(&fixture.repo(), &["checkout", "-q", "--detach"]);
    fixture.fail_gh();
    let mut args = vec!["--check", "cargo test"];
    args.extend(OTHER);

    let out = fixture.on_terminal(&args, "\n\n\n\n\n\n\nmain\n\n\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains(
            "origin/HEAD isn't set and HEAD is detached; check out the default branch \
             or run `git remote set-head origin <branch>`\r\nDefault branch: "
        ),
        "{shown}"
    );
    let prompt = fixture.herd_get("template.issue.prompt");
    assert!(prompt.contains("merges it into main"), "{prompt}");
    assert_eq!(fixture.gh_calls(), "");
}

#[test]
fn an_other_tracker_behind_an_ssh_host_alias_warns_to_pass_repo_and_without_an_origin_does_not() {
    for origin in [Some("git@github-work:acme/widgets.git"), None] {
        let fixture = Fixture::new(&["acme"]);
        match origin {
            Some(url) => fixture.set_origin(url),
            None => git(&fixture.repo(), &["remote", "remove", "origin"]),
        }
        fixture.commit_on("main");
        fixture.fail_gh();

        let out = fixture.init(&OTHER);

        assert_ok(&out);
        let hint = "note: if origin is GitHub behind an ssh host alias, pass --repo \
                    <owner>/<name> to keep the merged check";
        assert_eq!(
            text(&out.stderr).contains(hint),
            origin.is_some(),
            "{}",
            text(&out.stderr)
        );
    }
}

/// Replaces the fake gh with one that logs its call and fails.
fn fail_every_gh_call(fixture: &Fixture) {
    fs::write(
        fixture.gh_dir().join("gh"),
        "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/calls\"\nexit 1\n",
    )
    .unwrap();
}

#[test]
fn a_triage_role_mapped_twice_is_refused_before_any_gh_call_or_write() {
    for (first, second, message) in [
        (
            "| `ready-for-agent` | `AFK` | x |",
            "| `ready-for-agent` | `go` | x |",
            "maps ready-for-agent twice, to AFK and go",
        ),
        (
            "| `needs-info` | `Q:Info` | x |",
            "| `needs-info` | `ask` | x |",
            "maps needs-info twice, to Q:Info and ask",
        ),
        (
            "| `ready-for-human` | `HFK` | x |",
            "| `ready-for-human` | `mine` | x |",
            "maps ready-for-human twice, to HFK and mine",
        ),
        (
            "| `ready-for-agent` | `AFK` | x |",
            "| `Ready-For-Agent` | `go` | x |",
            "maps ready-for-agent twice, to AFK and go",
        ),
        (
            "| `needs-info` | `Q:Info` | x |",
            "| `needs-info` | `Q:Info` | x |",
            "maps needs-info twice, to Q:Info and Q:Info",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);
        write_docs(&fixture, None, &format!("{first}\n{second}\n"));
        fail_every_gh_call(&fixture);

        let out = fixture.init(&[]);

        assert_eq!(out.status.code(), Some(1), "{first} {second}");
        assert!(text(&out.stderr).contains(message), "{}", text(&out.stderr));
        assert_eq!(fixture.gh_calls(), "");
        assert!(!fixture.herds().exists());
        assert!(!fixture.repo().join("docs/agents/issue-tracker.md").exists());
    }
}

#[test]
fn a_role_loopherd_doesnt_read_may_repeat() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(
        &fixture,
        None,
        "| `needs-triage` | `triage` | x |\n\
         | `needs-triage` | `inbox` | x |\n\
         | `wontfix` | `no` | x |\n\
         | `wontfix` | `no` | x |\n",
    );

    assert_ok(&fixture.init(&[]));
}

#[test]
fn the_github_backlog_lists_open_issues_neither_ready_nor_blocked_keeping_those_on_the_human() {
    let fixture = Fixture::new(&["acme"]);
    fixture.gh_json(
        "issues.json",
        r#"[
  {"number": 12, "title": "Spec the widget API", "labels": []},
  {"number": 3, "title": "Ready already", "labels": [{"name": "Ready-For-Agent"}]},
  {"number": 4, "title": "Waits on 3", "labels": [{"name": "BLOCKED"}]},
  {"number": 5, "title": "Which colour?", "labels": [{"name": "needs-info"}]},
  {"number": 6, "title": "Pick a licence", "labels": [{"name": "ready-for-human"}, {"name": "bug"}]},
  {"number": 7, "title": "Asked mid-way", "labels": [{"name": "ready-for-agent"}, {"name": "Needs-Info"}]}
]"#,
    );
    assert_ok(&fixture.init(&[]));

    let backlog = fixture.run(
        Command::new("sh")
            .args(["-c", &fixture.herd_get("queue.backlog")])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fixture.gh_dir().display(),
                    std::env::var("PATH").unwrap()
                ),
            ),
    );

    assert_ok(&backlog);
    assert_eq!(
        text(&backlog.stdout),
        "5 Which colour?\n6 Pick a licence\n7 Asked mid-way\n12 Spec the widget API\n"
    );
    assert!(
        fixture
            .gh_calls()
            .contains("GH_TOKEN=tok-acme issue list --repo acme/widgets --state open --limit"),
        "{}",
        fixture.gh_calls()
    );
}

#[test]
fn the_local_backlog_lists_open_issue_files_neither_ready_nor_blocked_by_path_and_title() {
    let fixture = Fixture::new(&["acme"]);
    let mut issues = LOCAL_ISSUES.to_vec();
    issues.extend([
        (
            ".scratch/billing/issues/02-refunds.md",
            "# 02: Refunds\n\nStatus: blocked\n",
        ),
        (
            ".scratch/billing/issues/03-taxes.md",
            "No heading and no status yet.\n",
        ),
        (
            ".scratch/billing/issues/04-old.md",
            "# 04: Old\n\nStatus: closed\n",
        ),
        (
            ".scratch/billing/issues/05-credits.md",
            "# 05: Credits\n\nBlocked by: 01\nStatus: needs-triage\n",
        ),
        (
            ".scratch/billing/issues/06-coupons.md",
            "# 06: Coupons\n\nBlocked by: 04\nStatus: needs-triage\n",
        ),
    ]);
    let env = fixture.push_local_issues(&issues);
    assert_ok(&fixture.init(&["--tracker", "local"]));

    let backlog = fixture.run(
        Command::new("sh")
            .args(["-c", &fixture.herd_get("queue.backlog")])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );

    assert_ok(&backlog);
    assert_eq!(
        text(&backlog.stdout),
        ".scratch/auth/issues/03-session-expiry.md 03: Session expiry\n\
         .scratch/auth/issues/08-pick-a-provider.md 08: Pick a provider\n\
         .scratch/billing/issues/03-taxes.md 03-taxes\n\
         .scratch/billing/issues/06-coupons.md 06: Coupons\n"
    );
}

#[test]
fn another_trackers_backlog_is_left_to_fill_in_and_named_in_the_steps() {
    let fixture = Fixture::new(&["acme"]);
    fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
    fs::write(
        fixture.repo().join("docs/agents/issue-tracker.md"),
        "# Issue tracker: Jira\n",
    )
    .unwrap();

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(fixture.herd_get("queue.backlog"), "(fill in)");
    assert!(
        text(&out.stdout).contains("[queue] source, [queue] backlog, [template.issue] done"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn a_lead_with_merge_off_is_guarded_by_hook_unless_init_is_told_credentials() {
    let hook = Fixture::new(&["acme"]);
    assert_ok(&hook.init(&[]));
    assert_eq!(
        herd_toml(&hook)["lead"]["merge_guard"].as_str(),
        Some("hook")
    );

    let credentials = Fixture::new(&["acme"]);
    let out = credentials.init(&["--merge-guard", "credentials"]);

    assert_ok(&out);
    assert_eq!(
        herd_toml(&credentials)["lead"]["merge_guard"].as_str(),
        Some("credentials")
    );
    let stdout = text(&out.stdout);
    let setup = stdout.find("loopherd merge-guard widgets").expect(&stdout);
    let lead = stdout.find("loopherd lead widgets").expect(&stdout);
    assert!(setup < lead, "{stdout}");
}

#[test]
fn a_credentials_herd_runs_gh_on_the_leads_own_token_where_a_hook_herd_names_the_account() {
    let hook = Fixture::new(&["acme"]);
    assert_ok(&hook.init(&[]));
    let credentials = Fixture::new(&["acme"]);
    assert_ok(&credentials.init(&["--merge-guard", "credentials"]));

    for (fixture, gh) in [
        (&hook, "GH_TOKEN=$(gh auth token --user acme) gh "),
        (
            &credentials,
            "GH_TOKEN=${GH_TOKEN:-$(gh auth token --user acme)} gh ",
        ),
    ] {
        let herds = fixture.loopherd(&["herds"]);
        assert!(
            !text(&herds.stdout).contains("invalid"),
            "{}",
            text(&herds.stdout)
        );
        let herd = herd_toml(fixture);
        for check in [
            &herd["template"]["issue"]["merged"],
            &herd["template"]["issue"]["done"],
        ] {
            // A check is a template, where {{ and }} stand for braces.
            let check = check
                .as_str()
                .unwrap()
                .replace("{{", "{")
                .replace("}}", "}");
            assert!(check.starts_with(gh), "{check}");
        }
        for list in [&herd["queue"]["source"], &herd["queue"]["backlog"]] {
            let list = list.as_str().unwrap().trim_start().to_string();
            assert!(list.starts_with(gh), "{list}");
        }
    }
    let lead = fs::read_to_string(credentials.herds().join("widgets-lead.md")).unwrap();
    assert!(!lead.contains("gh auth token"), "{lead}");
    assert!(lead.contains("--admin --match-head-commit"), "{lead}");
}

#[test]
fn a_credentials_herds_workers_push_and_run_gh_on_the_herds_token_when_their_sandbox_gives_one() {
    for tracker in ["github", "local"] {
        let hook = Fixture::new(&["acme"]);
        hook.push_local_issues_to_plain_origin(LOCAL_ISSUES);
        assert_ok(&hook.init(&["--tracker", tracker, "--repo", "acme/widgets"]));
        let credentials = Fixture::new(&["acme"]);
        credentials.push_local_issues_to_plain_origin(LOCAL_ISSUES);
        assert_ok(&credentials.init(&[
            "--tracker",
            tracker,
            "--repo",
            "acme/widgets",
            "--merge-guard",
            "credentials",
        ]));

        for (fixture, token, not) in [
            (
                &hook,
                "GH_TOKEN=$(gh auth token --user acme)",
                "${GH_TOKEN:-",
            ),
            (
                &credentials,
                "GH_TOKEN=${GH_TOKEN:-$(gh auth token --user acme)}",
                "GH_TOKEN=$(gh auth token",
            ),
        ] {
            let herds = fixture.loopherd(&["herds"]);
            assert!(
                !text(&herds.stdout).contains("invalid"),
                "{}",
                text(&herds.stdout)
            );
            let herd = herd_toml(fixture);
            // The prompt is a template, where {{ and }} stand for braces.
            let prompt = herd["template"]["issue"]["prompt"]
                .as_str()
                .unwrap()
                .replace("{{", "{")
                .replace("}}", "}");
            for line in [
                format!(
                    "{token} git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD"
                ),
                format!("{token} gh pr create --repo acme/widgets"),
            ] {
                assert!(prompt.contains(&line), "{tracker}: {line}\n{prompt}");
            }
            assert!(!prompt.contains(not), "{tracker}: {prompt}");
        }
    }
}

#[test]
fn a_lead_with_merge_on_gets_no_merge_guard_and_refuses_one_given() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.config().join("config.toml"),
        "[lead_defaults]\nmerge = true\n",
    )
    .unwrap();

    let refused = fixture.init(&["--merge-guard", "credentials"]);

    assert_refused(
        &fixture,
        &refused,
        "loopherd init: --merge-guard is for a lead with merge off, and [lead_defaults] \
         turns merge on\n",
    );
    assert_ok(&fixture.init(&[]));
    let lead = herd_toml(&fixture)["lead"].clone();
    assert_eq!(lead.get("merge_guard"), None, "{lead}");
}

#[test]
fn on_a_terminal_init_explains_merge_guard_and_takes_credentials() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\nbogus\ncredentials\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("The lead has merge off. How is it kept from merging?"),
        "{shown}"
    );
    assert!(
        shown.contains("answer hook or credentials, not \"bogus\""),
        "{shown}"
    );
    assert!(
        shown.contains("lead    yes, merge_guard credentials"),
        "{shown}"
    );
    assert_eq!(
        herd_toml(&fixture)["lead"]["merge_guard"].as_str(),
        Some("credentials")
    );
}

fn groom(fixture: &Fixture) -> toml::Table {
    herd_toml(fixture)["template"]["groom"]
        .as_table()
        .expect("a [template.groom]")
        .clone()
}

#[test]
fn with_groom_off_a_terminal_writes_a_slotless_groom_template_feed_never_spawns() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--with", "groom"]);

    assert_ok(&out);
    let groom = groom(&fixture);
    let mut keys: Vec<&str> = groom.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["name", "params", "prompt", "slot", "worktree"]);
    assert_eq!(groom["params"].as_array().unwrap().len(), 0);
    assert_eq!(groom["name"].as_str(), Some("groom-{herd}"));
    assert_eq!(groom["worktree"].as_str(), Some("groom-{herd}"));
    assert_eq!(groom["slot"].as_bool(), Some(false));
    let herd = herd_toml(&fixture);
    assert_eq!(herd["queue"]["template"].as_str(), Some("issue"));
    assert!(!herd["queue"].as_table().unwrap().contains_key("templates"));
    let herds = fixture.loopherd(&["herds"]);
    assert_ok(&herds);
    assert!(
        text(&herds.stdout).contains("loopherd add widgets groom  (no slot)"),
        "{}",
        text(&herds.stdout)
    );
    assert!(
        !text(&herds.stdout).contains("warning"),
        "{}",
        text(&herds.stdout)
    );
}

#[test]
fn off_a_terminal_without_with_init_writes_no_groom_template() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&[]));

    assert!(herd_toml(&fixture)["template"].get("groom").is_none());
}

#[test]
fn the_github_groom_prompt_defers_to_the_skill_and_writes_batch_lines_as_gh_commands() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&["--with", "groom"]));

    let prompt = groom(&fixture)["prompt"].as_str().unwrap().to_string();
    let gh = "GH_TOKEN=$(gh auth token --user acme) gh";
    for expected in [
        "Run `loopherd --skill --section groom` and follow it".to_string(),
        "Read docs/agents/issue-tracker.md and AGENTS.md first.".into(),
        "`loopherd herd get {herd} queue.backlog` prints".into(),
        format!("`{gh} issue view <n> --repo acme/widgets --comments`"),
        format!("- mark ready: `{gh} issue edit <n> --repo acme/widgets --add-label ready-for-agent`"),
        format!("- needs spec: `{gh} issue comment <n> --repo acme/widgets --body \"<what's missing>\"`"),
        format!("- close: `{gh} issue close <n> --repo acme/widgets --reason \"not planned\"  # <reason>`"),
        "Never add ready-for-agent, close or move an issue, or add or remove needs-info or blocked."
            .into(),
        "loopherd report --kind done \"$(cat <<'EOF'\n<report>\nEOF\n)\"".into(),
    ] {
        assert!(prompt.contains(&expected), "missing {expected:?} in\n{prompt}");
    }
}

#[test]
fn the_local_groom_prompt_writes_batch_lines_that_edit_and_push_the_issue_file() {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();

    assert_ok(&fixture.init(&["--tracker", "local", "--with", "groom"]));

    let prompt = groom(&fixture)["prompt"].as_str().unwrap().to_string();
    let repo = fixture.repo().display().to_string();
    for expected in [
        "Run `loopherd --skill --section groom` and follow it".to_string(),
        "`loopherd herd get {herd} queue.backlog` prints".into(),
        format!(
            "Start each command with `cd '{repo}' && ([ \"$(git branch --show-current)\" = main ] \
             && git pull -q --ff-only origin main && [ -z \"$(git rev-list origin/main..HEAD)\" ] \
             && git diff --quiet HEAD -- <path> || (echo \"not run: check out main with no unpushed \
             commits or edits to <path>\" >&2; false)) && `\nand end it, before any `# <reason>`, \
             with ` && git push -q origin HEAD:main`"
        ),
        "- mark ready: `sed -i 's/^Status:.*/& ready-for-agent/' <path> && git commit -qm \
         \"Mark <path> ready\" -- <path>`"
            .into(),
        "- close: `sed -i 's/^Status:.*/Status: closed/' <path> && git commit -qm \
         \"Close <path>\" -- <path>  # <reason>`"
            .into(),
        "Never change a Status: line".into(),
    ] {
        assert!(
            prompt.contains(&expected),
            "missing {expected:?} in\n{prompt}"
        );
    }
    assert!(!prompt.contains("gh issue"), "{prompt}");
}

#[test]
fn the_other_trackers_groom_prompt_leaves_each_command_form_to_fill_in() {
    let fixture = Fixture::new(&["acme"]);

    assert_ok(&fixture.init(&[
        "--tracker",
        "other",
        "--tracker-desc",
        "Issues live in Linear.",
        "--with",
        "groom",
    ]));

    let prompt = groom(&fixture)["prompt"].as_str().unwrap().to_string();
    for expected in [
        "- mark ready: (fill in)",
        "- needs spec, a comment saying what's missing: (fill in)",
        "- close: (fill in)",
    ] {
        assert!(
            prompt.contains(expected),
            "missing {expected:?} in\n{prompt}"
        );
    }
}

#[test]
fn an_unknown_with_is_refused_before_anything_is_written() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&["--with", "bogus"]);

    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("bogus"), "{}", text(&out.stderr));
    assert!(
        fixture.config_listing().is_empty(),
        "{:?}",
        fixture.config_listing()
    );
    assert!(!fixture.repo().join("docs").exists());
}

#[test]
fn on_a_terminal_init_asks_for_a_groom_template_and_enter_adds_it() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains(
            "Add a groom template, to triage unready tasks when the herd runs low? [Y/n]: "
        ),
        "{shown}"
    );
    assert!(shown.contains("  groom   yes\r\n"), "{shown}");
    assert_eq!(groom(&fixture)["slot"].as_bool(), Some(false));
}

#[test]
fn on_a_terminal_answering_no_to_groom_writes_none() {
    let fixture = Fixture::new(&["acme"]);
    let typed = [
        "", "",  // repo, herd name
        "",  // tracker
        "",  // owner/name
        "",  // max
        "",  // lead
        "",  // merge_guard
        "",  // context_limit
        "",  // gh account
        "",  // default branch
        "n", // groom
        "",  // confirm
    ];

    let out = fixture.on_terminal(&["--check", "cargo test"], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("  groom   no\r\n"),
        "{}",
        text(&out.stdout)
    );
    assert!(herd_toml(&fixture)["template"].get("groom").is_none());
}

#[test]
fn on_a_terminal_with_groom_answers_the_question() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&["--with", "groom"], "\n");

    assert_ok(&out);
    assert!(
        !text(&out.stdout).contains("Add a groom template"),
        "{}",
        text(&out.stdout)
    );
    assert!(herd_toml(&fixture)["template"].get("groom").is_some());
}

#[test]
fn diff_reads_a_groom_template_back_and_shows_its_drift() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--with", "groom"]));
    assert_ok(&fixture.init(&["--diff"]));
    let herd_file = fixture.herds().join("widgets.toml");
    let edited = fs::read_to_string(&herd_file)
        .unwrap()
        .replace("worktree = \"groom-{herd}\"\n", "worktree = \"g\"\n");
    fs::write(&herd_file, &edited).unwrap();

    let out = fixture.init(&["--diff"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("\n-worktree = \"g\"\n+worktree = \"groom-{herd}\"\n"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn diff_with_groom_shows_the_groom_template_init_would_add() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let out = fixture.init(&["--diff", "--with", "groom"]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("\n+[template.groom]\n"),
        "{}",
        text(&out.stdout)
    );
}

/// The local groom prompt's mark-ready line for `path`, with the prefix and
/// suffix it says every command takes.
fn local_mark_ready(fixture: &Fixture, path: &str) -> String {
    let prompt = groom(fixture)["prompt"].as_str().unwrap().to_string();
    let between = |start: &str, end: &str| {
        let from = prompt
            .find(start)
            .unwrap_or_else(|| panic!("{start:?} in\n{prompt}"))
            + start.len();
        prompt[from..from + prompt[from..].find(end).unwrap()].to_string()
    };
    let prefix = between("Start each command with `", "`\n");
    let suffix = between("before any `# <reason>`, with `", "`");
    let form = between("- mark ready: `", "`\n");
    format!("{prefix}{form}{suffix}").replace("<path>", path)
}

fn origin_main(fixture: &Fixture) -> String {
    let origin = fixture.root.path().join("origin.git");
    let out = git_command(&origin)
        .args(["log", "--format=%s", "main"])
        .output()
        .unwrap();
    text(&out.stdout)
}

fn run_batch_line(fixture: &Fixture, line: &str) -> Output {
    let mut command = Command::new("sh");
    command
        .args(["-c", line])
        .current_dir(fixture.root.path())
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    isolate_git_config(&mut command).output().unwrap()
}

fn local_groom_fixture() -> Fixture {
    let fixture = Fixture::new(&["acme"]);
    fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
    fixture.fail_gh();
    git(
        &fixture.repo(),
        &["checkout", "-q", "-B", "main", "origin/main"],
    );
    assert_ok(&fixture.init(&["--tracker", "local", "--with", "groom"]));
    fixture
}

const LOGIN: &str = ".scratch/auth/issues/01-login-form.md";

#[test]
fn a_local_mark_ready_line_on_a_clean_up_to_date_main_pushes_only_its_own_commit() {
    let fixture = local_groom_fixture();
    let before = origin_main(&fixture);

    let out = run_batch_line(&fixture, &local_mark_ready(&fixture, LOGIN));

    assert_ok(&out);
    assert_eq!(
        origin_main(&fixture),
        format!("Mark {LOGIN} ready\n{before}")
    );
}

#[test]
fn a_local_mark_ready_line_refuses_a_checkout_with_an_unpushed_commit() {
    let fixture = local_groom_fixture();
    fs::write(fixture.repo().join("wip.txt"), "wip\n").unwrap();
    git(&fixture.repo(), &["add", "wip.txt"]);
    git(
        &fixture.repo(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "human WIP",
        ],
    );
    let before = origin_main(&fixture);

    let out = run_batch_line(&fixture, &local_mark_ready(&fixture, LOGIN));

    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("not run:"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(origin_main(&fixture), before);
}

#[test]
fn a_local_mark_ready_line_refuses_an_issue_file_with_uncommitted_edits() {
    let fixture = local_groom_fixture();
    let issue = fixture.repo().join(LOGIN);
    let edited = fs::read_to_string(&issue).unwrap() + "my edit\n";
    fs::write(&issue, &edited).unwrap();
    let before = origin_main(&fixture);

    let out = run_batch_line(&fixture, &local_mark_ready(&fixture, LOGIN));

    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("not run:"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(origin_main(&fixture), before);
    assert_eq!(fs::read_to_string(&issue).unwrap(), edited);
}

const GROOM_EXPLANATION: &str = "A groomer triages unready tasks when the herd runs low, for you to approve.\r\n\
     Say no if you keep the backlog ready yourself.\r\n";

#[test]
fn on_a_terminal_the_groom_question_shows_its_two_line_explanation_first() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    let question = "Add a groom template, to triage unready tasks when the herd runs low? [Y/n]: ";
    assert!(
        shown.contains(&format!("{GROOM_EXPLANATION}{question}")),
        "{shown}"
    );
    for line in GROOM_EXPLANATION.lines() {
        assert!(line.trim_end().chars().count() <= 80, "{line:?}");
    }
}

#[test]
fn on_a_terminal_each_question_explains_itself_above_its_prompt() {
    let fixture = Fixture::new(&["acme"]);
    fs::write(
        fixture.repo().join("Cargo.toml"),
        "[package]\nname = \"w\"\n",
    )
    .unwrap();

    let out = fixture.on_terminal(&[], &"\n".repeat(13));

    assert_ok(&out);
    let shown = text(&out.stdout);
    let at = |needle: &str| {
        shown
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in\n{shown}"))
    };
    let explained = at("How many workers the herd runs at once");
    let asked = at("max, how many workers run at once (3): ");
    assert!(explained < asked, "{shown}");
    assert!(
        !shown[explained..asked].contains("): "),
        "another prompt between max's explanation and its prompt:\n{shown}"
    );
    for (explanation, prompt) in [
        ("The checkout the herd", "Repo ("),
        ("Names the herd's file", "Herd name ("),
        (
            "Where the herd finds issues",
            "Where does this repo track issues?",
        ),
        ("The GitHub repo the herd", "GitHub repo (owner/name)"),
        ("Commands each worker runs", "Use these checks?"),
        ("A lead is a member", "Give the herd a lead"),
        ("How full a worker's context", "Workers' handoff limit"),
        ("The gh login the herd", "gh account ("),
        ("The branch workers cut", "Default branch ("),
        ("A groomer triages", "Add a groom template"),
    ] {
        assert!(at(explanation) < at(prompt), "{explanation:?}\n{shown}");
    }
}

#[test]
fn the_groom_explanation_is_left_out_when_with_groom_answers_or_off_a_terminal() {
    let fixture = Fixture::new(&["acme"]);
    let asked = fixture.init_on_terminal(&["--with", "groom"], "\n");
    assert_ok(&asked);
    assert!(
        !text(&asked.stdout).contains("A groomer triages"),
        "{}",
        text(&asked.stdout)
    );

    let other = Fixture::new(&["acme"]);
    let out = other.init(&[]);
    assert_ok(&out);
    assert!(
        !text(&out.stderr).contains("A groomer triages"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn without_a_terminal_init_explains_nothing() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init(&[]);

    assert_ok(&out);
    assert_eq!(text(&out.stderr), "");
}

fn grill_template(fixture: &Fixture) -> toml::Table {
    herd_toml(fixture)["template"]["grill"]
        .as_table()
        .unwrap()
        .clone()
}

#[test]
fn every_tracker_gets_an_attended_grill_with_the_issue_templates_done_and_merged() {
    let desc = "Issues live in Linear, team WID.";
    for (args, params, name, branch) in [
        (
            &["--tracker", "github"][..],
            &["number", "slug"][..],
            "{herd}-grill-{number}",
            "docs/{number}-{slug}",
        ),
        (
            &["--tracker", "local"][..],
            &["id", "slug", "path"][..],
            "{herd}-grill-{id}",
            "docs/{id}-{slug}",
        ),
        (
            &["--tracker", "local", "--no-github"][..],
            &["id", "slug", "path"][..],
            "{herd}-grill-{id}",
            "docs/{id}-{slug}",
        ),
        (
            &["--tracker", "other", "--tracker-desc", desc][..],
            &["number", "slug"][..],
            "{herd}-grill-{number}",
            "docs/{number}-{slug}",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);
        match args {
            [.., "--no-github"] => fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES),
            [_, "local"] => drop(fixture.push_local_issues(LOCAL_ISSUES)),
            _ => {}
        }
        assert_ok(&fixture.init(args));

        let grill = grill_template(&fixture);
        let issue = herd_toml(&fixture)["template"]["issue"].clone();
        let params: Vec<toml::Value> = params.iter().map(|p| (*p).into()).collect();
        assert_eq!(grill["params"], toml::Value::Array(params), "{args:?}");
        assert_eq!(grill["attended"], true.into(), "{args:?}");
        assert_eq!(grill["name"], name.into(), "{args:?}");
        assert_eq!(grill["branch"], branch.into(), "{args:?}");
        assert_eq!(grill["prompt_file"], "widgets-grill.md".into(), "{args:?}");
        assert_eq!(grill.get("done"), issue.get("done"), "{args:?}");
        assert_eq!(grill.get("merged"), issue.get("merged"), "{args:?}");
        assert_eq!(
            grill.contains_key("merged"),
            !args.contains(&"--no-github"),
            "{args:?}"
        );
        assert_eq!(
            grill.contains_key("done"),
            !args.contains(&"other"),
            "{args:?}"
        );
        if !args.contains(&"other") {
            assert_ok(&fixture.loopherd(&["herds"]));
        }
    }
}

#[test]
fn on_a_terminal_the_summary_lists_the_attended_grill() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert!(
        shown.contains("  groom   yes\r\n  also    grill (attended)\r\n"),
        "{shown}"
    );
}

#[test]
fn herds_shows_the_grill_attended_outside_the_queue_and_the_herd_valid() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let herds = fixture.loopherd(&["herds"]);

    assert_ok(&herds);
    let shown = text(&herds.stdout);
    assert!(
        shown.contains("  loopherd add widgets grill <number> <slug>  (attended)\n"),
        "{shown}"
    );
    assert!(
        shown.contains("  loopherd feed widgets  (template issue, max 3)\n"),
        "{shown}"
    );
    assert!(!shown.contains("warning"), "{shown}");
}

fn grill_prompt(fixture: &Fixture) -> String {
    fs::read_to_string(fixture.herds().join("widgets-grill.md")).unwrap()
}

fn assert_has(text: &str, expected: &[&str]) {
    for expected in expected {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
}

#[test]
fn the_grill_prompt_claims_waits_grills_records_hands_in_and_focuses_the_shepherd() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let prompt = grill_prompt(&fixture);

    assert_has(
        &prompt,
        &[
            "You're {name}, grilling GitHub issue #{number} of acme/widgets\n\
             live with the human in this pane, on branch {branch} in {worktree}.",
            "`GH_TOKEN=$(gh auth token --user acme) gh issue view {number} --repo acme/widgets --comments`",
            "Claim it first",
            "`GH_TOKEN=$(gh auth token --user acme) gh issue comment {number} --repo acme/widgets \
             --body \"claimed by {name}\"`",
            "--add-assignee acme`",
            "Once claimed, end your turn without asking anything: the shepherd sends\n\
             background next with `loopherd tell`",
            "invoke the `grilling` skill",
            "If it isn't\ninstalled: ask one question at a time, each with your recommended answer;",
            "(\"batch the rest\")",
            "(\"discuss 3\")",
            "where AGENTS.md or the repo's docs say decisions go",
            "`docs/adr/<NNNN>-<slug>.md`",
            "Show the human each implementation issue you'd file, in full, before\n  filing it",
            "suggest `/wayfinder`\n(mattpocock-skills)",
            "push -u origin HEAD`",
            "gh pr create --repo acme/widgets --base main --head {branch} --assignee acme",
            "the body ending with the line `Closes #{number}`.",
            "`herdr agent focus {shepherd_pane}`",
            "Do no close-out after the merge",
        ],
    );
    let claim = prompt.find("Claim it first").unwrap();
    let wait = prompt.find("Once claimed, end your turn").unwrap();
    let grill = prompt.find("invoke the `grilling` skill").unwrap();
    assert!(claim < wait && wait < grill, "{prompt}");
}

#[test]
fn the_grill_fallback_is_at_most_four_lines() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&[]));

    let prompt = grill_prompt(&fixture);

    let fallback = prompt
        .split("\n\n")
        .find(|paragraph| paragraph.contains("If it isn't\ninstalled"))
        .unwrap();
    assert!(fallback.lines().count() <= 4, "{fallback}");
}

#[test]
fn a_local_grill_resolves_its_issue_on_the_branch_and_hands_it_in_as_the_herd_does() {
    for (no_github, hand_in) in [
        (
            false,
            "gh pr create --repo acme/widgets --base main --head {branch} --assignee acme",
        ),
        (
            true,
            "push the branch with\n  `git push -u origin HEAD`. Open no pull request",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);
        let mut args = vec!["--tracker", "local"];
        match no_github {
            true => {
                fixture.push_local_issues_to_plain_origin(LOCAL_ISSUES);
                args.push("--no-github");
            }
            false => drop(fixture.push_local_issues(LOCAL_ISSUES)),
        }
        assert_ok(&fixture.init(&args));

        let prompt = grill_prompt(&fixture);

        assert_has(
            &prompt,
            &[
                "grilling the local issue {path}",
                "then {path} in full, comments\nincluded.",
                "set its `Status:` line to\n`claimed`, append `claimed by {name}`",
                "set {path}'s `Status:` line to\n  `resolved`",
                hand_in,
            ],
        );
        assert_eq!(prompt.contains("gh "), !no_github, "{prompt}");
    }
}

#[test]
fn an_other_trackers_grill_prompt_marks_each_tracker_command_to_fill_in() {
    let fixture = Fixture::new(&["acme"]);
    assert_ok(&fixture.init(&["--tracker", "other", "--tracker-desc", "Linear"]));

    let prompt = grill_prompt(&fixture);

    assert_has(
        &prompt,
        &[
            "then task {number} and every comment on it. (fill in)",
            "comment `claimed by {name}` on task {number}. (fill in)",
            "Link it on task {number}, so the task closes when the PR merges. (fill in)",
        ],
    );
}
