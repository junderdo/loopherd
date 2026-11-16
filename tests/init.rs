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

    /// `loopherd init` under a file-size limit that `file`, the lead prompt or
    /// the herd file, outgrows; the limit comes from what init writes here.
    fn init_failing_to_write(&self, file: &str) -> Output {
        assert_ok(&self.init(&[]));
        let size = |name: &str| fs::metadata(self.herds().join(name)).unwrap().len();
        let (lead, herd) = (size("widgets-lead.md"), size("widgets.toml"));
        fs::remove_dir_all(self.herds()).unwrap();
        // ulimit -f counts 512-byte blocks.
        let blocks = match file {
            "widgets-lead.md" => (lead - 1) / 512,
            _ => lead.div_ceil(512),
        };
        assert!(
            file == "widgets-lead.md" || herd > blocks * 512,
            "the herd file ({herd} bytes) fits in the lead prompt's {blocks} blocks"
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
        command
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.state())
            .env_remove("HERDR_PANE_ID")
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
             blocked_dialogs off (herd file)"
        ),
        "{herds}"
    );
    let lead = fs::read_to_string(fixture.herds().join("widgets-lead.md")).unwrap();
    assert!(lead.contains("{switches}"), "{lead}");
    assert!(
        lead.contains(
            "re-arm after every exit. On `<member> merged`, run `loopherd feed {herd}`,\n   \
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
fn repo_refuses_a_name_that_is_a_path_step_an_option_or_has_a_git_suffix() {
    for slug in [
        "acme/.",
        "acme/..",
        "acme/-widgets",
        "-acme/widgets",
        ".acme/widgets",
        "acme/widgets.git",
    ] {
        let fixture = Fixture::new(&["acme"]);

        assert_refused(
            &fixture,
            &fixture.init(&[&format!("--repo={slug}")]),
            &format!("loopherd init: --repo {slug:?} is not <owner>/<name>\n"),
        );
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
    let log = Command::new("git")
        .args(["rev-list", "--all", "--count"])
        .current_dir(fixture.repo())
        .output()
        .unwrap();
    assert_eq!(text(&log.stdout), "0\n");
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(fixture.repo())
        .output()
        .unwrap();
    assert_eq!(
        text(&status.stdout),
        "?? AGENTS.md\n?? docs/agents/issue-tracker.md\n?? docs/agents/loopherd.md\n\
         ?? docs/agents/triage-labels.md\n"
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
            "Fill [queue] source in {} from docs/agents/issue-tracker.md",
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
    assert!(herd.contains("--add-label Needs {{more}} info"), "{herd}");
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
            "  2. Fill [queue] source in {} from docs/agents/issue-tracker.md",
            fixture.herds().join("widgets.toml").display()
        )),
        "{next}"
    );
    assert!(
        next.contains("adapt its issue template and docs/agents/loopherd.md to the same tracker"),
        "{next}"
    );
    assert!(
        next.contains("  3. Tailor the herd to the repo per \"Setting up a project\""),
        "{next}"
    );
    assert!(
        next.contains("  4. Review and commit what init wrote in the repo"),
        "{next}"
    );
    assert!(next.contains("  7. loopherd lead widgets"), "{next}");
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
    /// `loopherd init --check 'cargo test'` on a terminal, typing Enter up to
    /// the tracker question, then `input`, then Enter for the rest.
    fn init_on_terminal(&self, extra: &[&str], input: &str) -> Output {
        let mut args = vec!["--check", "cargo test"];
        args.extend(extra);
        self.on_terminal(&args, &format!("\n\n{input}\n\n\n\n\n\n\n"))
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

    let out = fixture.init_on_terminal(&["--tracker", "local"], "");

    assert_ok(&out);
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
            "   - `cargo fmt --check`\n   - `cargo clippy -- -D warnings`\n   - `cargo test`\n"
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
        "170000", "100000", // context_limit
        "bot",    // gh account
        "trunk",  // default branch
        "",       // confirm
    ];

    let out = fixture.on_terminal(&[], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    let shown = text(&out.stdout);
    for expected in [
        format!("Repo ({}): ", fixture.repo().display()),
        "Herd name (widgets): ".into(),
        "GitHub repo (owner/name) (acme/widgets): ".into(),
        "  cargo fmt --check\r\n  cargo clippy -- -D warnings\r\n  cargo test\r\n".into(),
        "Use these checks? [Y/n]: ".into(),
        "max, how many workers run at once (3): ".into(),
        "max must be a positive integer, got \"0\"".into(),
        "Give the herd a lead, a member that feeds, watches and verifies its workers? [Y/n]: "
            .into(),
        "Workers' handoff limit in tokens (context_limit); Enter leaves it unset, so \
         the default 160000 applies: "
            .into(),
        "context_limit 170000 is over 160000, 80% of the 200000-token context window".into(),
        "gh account (logged in: acme, bot) (acme): ".into(),
        "Default branch (main): ".into(),
        "Herd gadgets:".into(),
        "  max     4 workers at once".into(),
        "  lead    no".into(),
        "  limit   100000".into(),
        "  checks  cargo fmt --check; cargo clippy -- -D warnings; cargo test".into(),
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

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\nn\n");

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
    let typed = "\n\n\n\n\nmake lint\nmake test\n\n\n\n\n\n\n\n";

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
    ];

    let out = fixture.on_terminal(&[], &(typed.join("\n") + "\n"));

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains("Check (cargo clippy -- -D warnings): "),
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
    let typed = ["", "", "gadgets", "", "", "", "y", "n", "", "", "", ""];

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

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\nma\x27\x27\x27in\ntrunk\n\n");

    assert_ok(&out);
    assert!(text(&out.stdout).contains("can't go in the worker prompt"));
    let herd = fs::read_to_string(fixture.herds().join("widgets.toml")).unwrap();
    assert!(herd.contains("--base trunk"), "{herd}");
}

#[test]
fn on_a_terminal_an_unmatched_account_is_asked_from_the_logged_in_ones() {
    let fixture = Fixture::new(&["bot", "other"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\nnobody\nother\n");

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
        text(&out.stdout).contains("  limit   unset, the default 160000 applies"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn on_a_terminal_a_limit_of_exactly_80_percent_is_taken_and_one_more_asked_again() {
    let fixture = Fixture::new(&["acme"]);

    let out = fixture.init_on_terminal(&[], "\n\n\n\n160001\n160000\n");

    assert_ok(&out);
    let shown = text(&out.stdout);
    assert_eq!(
        shown.matches("context_limit 160001 is over 160000").count(),
        1,
        "{shown}"
    );
    assert_eq!(
        herd_toml(&fixture)["template"]["issue"]["context_limit"].as_integer(),
        Some(160000)
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

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\nma in\ntrunk\n\n");

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
        &format!("{}\n\n\n\n\n\n\n\n\n\n\n", outside.display()),
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
             docs/agents/loopherd.md and AGENTS.md in the repo, uncommitted.\r\n"
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
        &format!("{}\n\n\n\n\n\n\n\n\n\n\n", other.display()),
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
        "default_context_limit = 500000\n",
    )
    .unwrap();

    let out = fixture.init_on_terminal(&[], "\n");

    assert_ok(&out);
    assert!(
        text(&out.stdout).contains(
            "  limit   unset, the cap of 160000 (80% of the default 200000-token context \
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
            "wrote docs/agents/loopherd.md (uncommitted)\nwrote AGENTS.md (uncommitted)\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Review and commit what init wrote in the repo: docs/agents/issue-tracker.md, \
             docs/agents/triage-labels.md, docs/agents/loopherd.md and AGENTS.md."
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
            "docs/agents/loopherd.md and a loopherd section in AGENTS.md in the repo, uncommitted."
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

    let out = fixture.init_on_terminal(&[], "\n\n\n\n\n\n\nn\n");

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
        "would add widgets-auth-01 (template issue)\n\
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
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = fetch ] && [ ! -e {lost} ]; then\n  : > {lost}\n  \
             echo \"error: cannot lock ref 'refs/remotes/origin/main'\" >&2\n  exit 1\nfi\n\
             exec {real_git} \"$@\"\n",
            lost = lost.display(),
            real_git = real_git.trim(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
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
