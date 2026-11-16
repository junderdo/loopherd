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
    assert_eq!(fs::read_dir(fixture.herds()).unwrap().count(), 0);
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
    assert_eq!(fs::read_dir(fixture.herds()).unwrap().count(), 0);
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

    assert_ok(&fixture.init(&[]));

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
        "?? docs/agents/issue-tracker.md\n?? docs/agents/triage-labels.md\n"
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
         | `ready-for-human` | `hfk` | x |\n",
    )
    .unwrap();

    assert_ok(&fixture.init(&[]));

    let calls = fixture.gh_calls();
    for label in ["AFK", "Q:Info", "hfk", "blocked"] {
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
    for (label, message) in [
        ("needs info", "maps needs-info to \"needs info\""),
        ("-x", "maps needs-info to \"-x\""),
        (
            "Blocked",
            "maps needs-info to blocked, which is loopherd's own label",
        ),
    ] {
        let fixture = Fixture::new(&["acme"]);
        fs::create_dir_all(fixture.repo().join("docs/agents")).unwrap();
        fs::write(
            fixture.repo().join("docs/agents/triage-labels.md"),
            format!("| `needs-info` | `{label}` | x |\n"),
        )
        .unwrap();

        let out = fixture.init(&[]);

        assert_eq!(out.status.code(), Some(1), "{label}");
        assert!(text(&out.stderr).contains(message), "{}", text(&out.stderr));
        assert_eq!(fixture.gh_calls(), "");
        assert!(!fixture.herds().join("widgets.toml").exists());
    }
}

#[test]
fn tracker_names_the_doc_init_writes_when_there_is_none() {
    for (args, title, github) in [
        (
            &["--tracker", "github"][..],
            "# Issue tracker: GitHub\n",
            true,
        ),
        (
            &["--tracker", "local"][..],
            "# Issue tracker: Local Markdown\n",
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
            Some(if github { 0 } else { 1 }),
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
    assert_eq!(fs::read_dir(fixture.herds()).unwrap().count(), 0);
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
fn a_tracker_other_than_github_takes_labels_no_shell_command_runs() {
    let fixture = Fixture::new(&["acme"]);
    write_docs(
        &fixture,
        Some("# Issue tracker: Local Markdown\n"),
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

    let out = fixture.init(&["--tracker", "local"]);

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
    assert!(next.contains("  5. loopherd lead widgets"), "{next}");
}
