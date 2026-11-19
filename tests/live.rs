//! End-to-end runs of `loopherd spawn`, `add`, `revive`, `retire --close`, `feed`, the Stop hook, `watch` and the herd view's event subscriber against the real `loopherd-dev` herdr
//! session, `herdr plugin link` of `herdr-plugin/` into a throwaway herdr home, and its view popup on a throwaway herdr server. Skipped unless `LOOPHERD_LIVE=1`; start the session first with
//! `scripts/herdr-dev --start`, then run through it:
//! `LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

// The binary crate has no library target, so the subscriber is included whole.
#[allow(dead_code)]
#[path = "../src/herdr/events.rs"]
mod events;

const HERDR_IDS: [&str; 4] = [
    "HERDR_ENV",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

fn live_enabled() -> bool {
    let on = std::env::var("LOOPHERD_LIVE").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set LOOPHERD_LIVE=1 to run against loopherd-dev");
    }
    on
}

fn live_socket() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join(".config/herdr/herdr.sock")
}

fn dev_socket() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
        .join(".config/herdr/sessions/loopherd-dev/herdr.sock")
}

/// A command whose herdr calls reach only the dev session, and which never
/// reads the human's loopherd config or measures the calling Claude.
fn dev(program: impl AsRef<std::ffi::OsStr>) -> Command {
    static CONFIG: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let config = CONFIG.get_or_init(|| tempfile::tempdir().unwrap());
    let mut cmd = Command::new(program);
    cmd.env("HERDR_SOCKET_PATH", dev_socket())
        .env("LOOPHERD_CONFIG_DIR", config.path())
        .env_remove("CLAUDE_PID");
    for var in HERDR_IDS {
        cmd.env_remove(var);
    }
    cmd
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn labels(out: Output) -> Vec<String> {
    json(&out)["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["label"].as_str().unwrap().to_string())
        .collect()
}

/// Passes only when the run inherited the dev socket, i.e. it was launched
/// through `scripts/herdr-dev`, which checks the dev session against the live
/// one so the tests never have to call the live session themselves, and the
/// dev socket is not the live socket under another name.
fn check_inherited_socket(inherited: Option<&Path>, dev: &Path, live: &Path) -> Result<(), String> {
    match inherited {
        Some(socket) if socket == dev && same_file(dev, live) => Err(format!(
            "dev socket {} resolves to the live socket {}",
            dev.display(),
            live.display()
        )),
        Some(socket) if socket == dev => Ok(()),
        Some(socket) => Err(format!(
            "HERDR_SOCKET_PATH is {}, not the dev socket; run via scripts/herdr-dev",
            socket.display()
        )),
        None => Err("HERDR_SOCKET_PATH is unset; run via scripts/herdr-dev".into()),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// Fails unless the run came through `scripts/herdr-dev` and herdr resolves
/// to the running dev session. Returns a dev workspace label.
fn assert_dev_session() -> String {
    let inherited = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    if let Err(e) = check_inherited_socket(inherited.as_deref(), &dev_socket(), &live_socket()) {
        panic!("{e}");
    }

    let status = dev("herdr").arg("status").output().unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    let socket_line = format!("  socket: {}", dev_socket().display());
    assert!(
        status.lines().any(|l| l == socket_line),
        "herdr is not targeting the dev socket:\n{status}"
    );
    assert!(
        status.contains("status: running"),
        "loopherd-dev is not running; start it with scripts/herdr-dev --start"
    );

    labels(dev("herdr").args(["workspace", "list"]).output().unwrap())
        .into_iter()
        .next()
        .expect("loopherd-dev has no workspace")
}

/// Closes the agent's tab when dropped, so a failed assertion leaves no tab.
struct CloseTab(String);

impl Drop for CloseTab {
    fn drop(&mut self) {
        let out = dev("herdr")
            .args(["agent", "get", &self.0])
            .output()
            .unwrap();
        if out.status.success() {
            let tab = json(&out)["result"]["agent"]["tab_id"]
                .as_str()
                .unwrap()
                .to_string();
            dev("herdr").args(["tab", "close", &tab]).output().unwrap();
        }
    }
}

/// Every real Claude the suite starts runs on Haiku, to keep live runs cheap.
const MODEL: [&str; 2] = ["--model", "haiku"];

/// `claude_args` go to the member's Claude after [`MODEL`].
fn spawn(state: &Path, args: &[&str], claude_args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .arg("spawn")
        .args(args)
        .arg("--")
        .args(MODEL)
        .args(claude_args)
        .output()
        .unwrap()
}

/// The head of a temp herd file, running its members on [`MODEL`].
fn herd_head(repo: &str, workspace: &str) -> String {
    format!("repo = {repo:?}\nworkspace = {workspace:?}\nargs = {MODEL:?}\n")
}

fn add(state: &Path, config: &Path, args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", config)
        .arg("add")
        .args(args)
        .output()
        .unwrap()
}

fn revive(state: &Path, args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .arg("revive")
        .args(args)
        .output()
        .unwrap()
}

fn retire_close(state: &Path, config: &Path, name: &str) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", config)
        // The member never reports, so only --force gets it past retire's check.
        .args(["retire", name, "--close", "--force"])
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A clone at `<root>/proj/proj` of a bare origin with one commit on `main`.
fn repo_with_origin(root: &Path) -> PathBuf {
    let origin = root.join("origin.git");
    let repo = root.join("proj/proj");
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "root"]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "--quiet", "-u", "origin", "main"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    repo
}

fn herdr_ok(args: &[&str]) -> Value {
    let out = dev("herdr").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "herdr {}: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    json(&out)
}

fn ledger_entry(state: &Path, name: &str) -> Value {
    let text = std::fs::read_to_string(state.join("loopherd/ledger.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()[name].clone()
}

#[test]
fn spawns_a_ready_agent_and_submits_its_prompt() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let name = format!("lh-live-ready-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());

    let out = spawn(
        state.path(),
        &[
            "--repo",
            env!("CARGO_MANIFEST_DIR"),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--prompt",
            "Reply with just the word ok.",
        ],
        &[],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("tab "), "{stdout}");
    assert!(stdout.contains("\npane "), "{stdout}");
    let entry = ledger_entry(state.path(), &name);
    assert_eq!(entry["prompt_sent"], true);
    assert!(entry["session_id"].is_string(), "{entry}");
    assert!(stdout.contains(entry["session_id"].as_str().unwrap()));
}

#[test]
fn leaves_an_agent_blocked_at_a_dialog_and_records_it_unprompted() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    // A fresh directory makes Claude ask whether to trust it.
    let untrusted = tempfile::tempdir().unwrap();
    let name = format!("lh-live-blocked-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());

    let out = spawn(
        state.path(),
        &[
            "--repo",
            untrusted.path().to_str().unwrap(),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--prompt",
            "never sent",
            "--ready-timeout",
            "5",
        ],
        &[],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("tab "), "{stdout}");
    assert!(stdout.contains("\nblocked: answer it in pane "), "{stdout}");
    assert_eq!(ledger_entry(state.path(), &name)["prompt_sent"], false);
}

#[test]
fn revives_an_agent_whose_tab_was_closed_under_its_name_and_session() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let name = format!("lh-live-revive-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());

    let out = spawn(
        state.path(),
        &[
            "--repo",
            env!("CARGO_MANIFEST_DIR"),
            "--name",
            &name,
            "--workspace",
            &workspace,
        ],
        &[],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // claude --resume finds no conversation until a turn has been taken.
    herdr_ok(&[
        "agent",
        "prompt",
        &name,
        "Reply with just the word ok. Do not use tools.",
        "--wait",
        "--timeout",
        "120000",
    ]);
    let session = ledger_entry(state.path(), &name)["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let tab = herdr_ok(&["agent", "get", &name])["result"]["agent"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    herdr_ok(&["tab", "close", &tab]);

    let out = revive(state.path(), &["--name", &name]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let prefix = format!("resumed {name} ");
    assert!(stdout.starts_with(&prefix), "{stdout}");
    assert!(
        stdout.contains(&format!("(prompt not sent, run loopherd prompt {name})")),
        "{stdout}"
    );
    let agent = &herdr_ok(&["agent", "get", &name])["result"]["agent"];
    assert_eq!(agent["agent_session"]["value"], session.as_str());
    assert_ne!(agent["tab_id"], tab.as_str());
    let pane = agent["pane_id"].as_str().unwrap();
    assert!(stdout.starts_with(&format!("{prefix}{pane}")), "{stdout}");
    assert_eq!(ledger_entry(state.path(), &name)["pane_id"], pane);

    let again = revive(state.path(), &["--name", &name]);
    assert_eq!(
        String::from_utf8_lossy(&again.stdout).lines().next(),
        Some(format!("alive {name} {pane} (prompt not sent, run loopherd prompt {name})").as_str())
    );
}

#[test]
fn adds_a_member_from_a_temp_herd_file() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    std::fs::create_dir(&herds).unwrap();
    let herd = format!("lh-live-herd-{}", std::process::id());
    std::fs::write(
        herds.join(format!("{herd}.toml")),
        format!(
            "{}[template.echo]\nparams = [\"n\"]\nname = \"lh-live-add-{{n}}\"\n\
             prompt = \"You're {{name}} in {{herd}}. Reply with just the word ok.\"\n",
            herd_head(env!("CARGO_MANIFEST_DIR"), &workspace)
        ),
    )
    .unwrap();
    let n = std::process::id().to_string();
    let name = format!("lh-live-add-{n}");
    let _cleanup = CloseTab(name.clone());

    let out = add(state.path(), config.path(), &[&herd, "echo", &n]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("tab "), "{stdout}");
    let entry = ledger_entry(state.path(), &name);
    assert_eq!(entry["herd"], herd.as_str());
    assert_eq!(entry["template"], "echo");
    assert_eq!(entry["prompt_sent"], true);
    assert_eq!(
        entry["prompt"],
        format!("You're {name} in {herd}. Reply with just the word ok.").as_str()
    );
}

#[test]
fn retire_close_closes_the_tab_and_removes_a_clean_merged_worktree() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let repo = repo_with_origin(root.path());
    let name = format!("lh-live-close-{}", std::process::id());
    let branch = format!("feat/{name}");
    let wt = root.path().join(format!("proj/worktrees/feat-{name}"));
    let _cleanup = CloseTab(name.clone());

    // A fresh repo stops Claude at its trust dialog; the tab and worktree are what matter.
    let out = spawn(
        state.path(),
        &[
            "--repo",
            repo.to_str().unwrap(),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--worktree",
            &branch,
            "--ready-timeout",
            "5",
        ],
        &[],
    );
    assert!(
        matches!(out.status.code(), Some(0 | 3)),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tab = herdr_ok(&["agent", "get", &name])["result"]["agent"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(wt.is_dir());

    let out = retire_close(state.path(), config.path(), &name);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        format!(
            "retired {name}\nclosed tab {tab}\nremoved {}\n",
            wt.display()
        )
    );
    assert_eq!(ledger_entry(state.path(), &name)["retired"], true);
    let tabs = herdr_ok(&["tab", "list"]);
    assert!(
        !tabs["result"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["tab_id"] == tab.as_str()),
        "{tabs}"
    );
    assert!(!wt.exists());
    assert!(repo.join(".git").is_dir());
    let retired_at = ledger_entry(state.path(), &name)["retired_at"].clone();
    // Retire times have one-second resolution, so a fast rerun could hide a rewrite.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let rerun = retire_close(state.path(), config.path(), &name);

    let stdout = String::from_utf8_lossy(&rerun.stdout);
    assert_eq!(rerun.status.code(), Some(0), "{stdout}");
    assert_eq!(ledger_entry(state.path(), &name)["retired_at"], retired_at);
    assert_eq!(
        stdout,
        format!(
            "{name} is already retired (since {})\ntab already gone\nworktree {} already gone\n",
            retired_at.as_str().unwrap(),
            wt.display()
        )
    );
}

fn feed(state: &Path, config: &Path, herd: &str) -> Output {
    feed_with(state, config, &[herd])
}

fn feed_with(state: &Path, config: &Path, args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", config)
        .arg("feed")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn feed_fills_a_slot_then_reaps_the_member_once_its_done_check_passes() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    std::fs::create_dir(&herds).unwrap();
    let herd = format!("lh-live-feed-{}", std::process::id());
    let tasks = work.path().join("tasks");
    let done = work.path().join("done");
    std::fs::write(
        herds.join(format!("{herd}.toml")),
        format!(
            "{}[template.echo]\nparams = [\"n\"]\nname = \"{{herd}}-{{n}}\"\n\
             prompt = \"Reply with just the word ok.\"\ndone = \"test -f {}-{{n}}\"\n\
             [queue]\ntemplate = \"echo\"\nsource = \"cat {}\"\nmax = 1\n",
            herd_head(env!("CARGO_MANIFEST_DIR"), &workspace),
            done.display(),
            tasks.display()
        ),
    )
    .unwrap();
    let name = format!("{herd}-1");
    let _cleanup = CloseTab(name.clone());
    std::fs::write(&tasks, "1\n").unwrap();

    let out = feed(state.path(), config.path(), &herd);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        format!("added {name}\n{herd}: 0 retired, 1 added, 0 skipped, 0 errors\n")
    );
    let tab = herdr_ok(&["agent", "get", &name])["result"]["agent"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = feed(state.path(), config.path(), &herd);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        format!(
            "still open {name}\nskipped: exists {name}\n{herd}: 0 retired, 0 added, 1 skipped, 0 errors\n"
        )
    );

    std::fs::write(format!("{}-1", done.display()), "").unwrap();
    let out = feed(state.path(), config.path(), &herd);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert_eq!(
        stdout,
        format!(
            "retired {name}; closed tab {tab}\nskipped: exists {name}\n\
             {herd}: 1 retired, 0 added, 1 skipped, 0 errors\n"
        )
    );
    assert_eq!(ledger_entry(state.path(), &name)["retired"], true);
    let tabs = herdr_ok(&["tab", "list"]);
    assert!(
        !tabs["result"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["tab_id"] == tab.as_str()),
        "{tabs}"
    );
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

/// A member fed on its own branch in a fresh repo, waiting on its done file.
struct BranchMember {
    state: tempfile::TempDir,
    config: tempfile::TempDir,
    root: tempfile::TempDir,
    repo: PathBuf,
    herd: String,
    name: String,
    branch: String,
    wt: PathBuf,
    tab: String,
    _cleanup: CloseTab,
}

impl BranchMember {
    fn feed(&self) -> Output {
        feed_with(self.state.path(), self.config.path(), &[&self.herd])
    }

    fn finish(&self) {
        std::fs::write(self.root.path().join("done-1"), "").unwrap();
    }
}

fn feed_a_branch_member(kind: &str) -> BranchMember {
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let repo = repo_with_origin(root.path());
    let herds = config.path().join("herds");
    std::fs::create_dir(&herds).unwrap();
    let herd = format!("lh-live-{kind}-{}", std::process::id());
    let tasks = root.path().join("tasks");
    std::fs::write(&tasks, "1\n").unwrap();
    std::fs::write(
        herds.join(format!("{herd}.toml")),
        format!(
            "{}[template.echo]\nparams = [\"n\"]\nname = \"{{herd}}-{{n}}\"\n\
             branch = \"feat/{{name}}\"\nprompt = \"Reply with just the word ok.\"\n\
             done = \"test -f {}/done-{{n}}\"\n\
             [queue]\ntemplate = \"echo\"\nsource = \"cat {}\"\nmax = 1\n",
            herd_head(repo.to_str().unwrap(), &workspace),
            root.path().display(),
            tasks.display()
        ),
    )
    .unwrap();
    let name = format!("{herd}-1");
    let branch = format!("feat/{name}");
    let wt = root.path().join(format!("proj/worktrees/feat-{name}"));
    let cleanup = CloseTab(name.clone());

    // A fresh repo stops Claude at its trust dialog; the tab and worktree are what matter.
    let out = feed_with(
        state.path(),
        config.path(),
        &[&herd, "--ready-timeout", "5"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        matches!(out.status.code(), Some(0 | 3)),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with(&format!("added {name}\n")), "{stdout}");
    let tab = herdr_ok(&["agent", "get", &name])["result"]["agent"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(wt.is_dir());
    assert!(branch_exists(&repo, &branch));

    BranchMember {
        state,
        config,
        root,
        repo,
        herd,
        name,
        branch,
        wt,
        tab,
        _cleanup: cleanup,
    }
}

#[test]
fn feed_reaps_a_member_and_removes_its_clean_merged_worktree_and_branch() {
    if !live_enabled() {
        return;
    }
    let m = feed_a_branch_member("feed-wt");

    let out = m.feed();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "still open {0}\nskipped: exists {0}\n{1}: 0 retired, 0 added, 1 skipped, 0 errors\n",
            m.name, m.herd
        )
    );
    assert!(m.wt.is_dir());

    m.finish();
    let out = m.feed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        format!(
            "retired {0}; closed tab {1}; removed {2}\nskipped: exists {0}\n\
             {3}: 1 retired, 0 added, 1 skipped, 0 errors\n",
            m.name,
            m.tab,
            m.wt.display(),
            m.herd
        )
    );
    assert_eq!(ledger_entry(m.state.path(), &m.name)["retired"], true);
    assert!(!m.wt.exists());
    assert!(!branch_exists(&m.repo, &m.branch));
    assert!(m.repo.join(".git").is_dir());
}

#[test]
fn feed_reaps_a_member_but_keeps_its_dirty_worktree_and_says_why() {
    if !live_enabled() {
        return;
    }
    let m = feed_a_branch_member("feed-dirty");
    std::fs::write(m.wt.join("scratch.txt"), "unsaved work").unwrap();

    m.finish();
    let out = m.feed();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout,
        format!(
            "retired {0}; closed tab {1}; kept {2}: uncommitted or untracked changes\n\
             skipped: exists {0}\n{3}: 1 retired, 0 added, 1 skipped, 0 errors\n",
            m.name,
            m.tab,
            m.wt.display(),
            m.herd
        )
    );
    assert_eq!(ledger_entry(m.state.path(), &m.name)["retired"], true);
    assert!(m.wt.join("scratch.txt").is_file());
    assert!(branch_exists(&m.repo, &m.branch));
}

/// Closes a tab by id when dropped.
struct CloseTabId(String);

impl Drop for CloseTabId {
    fn drop(&mut self) {
        dev("herdr")
            .args(["tab", "close", &self.0])
            .output()
            .unwrap();
    }
}

#[test]
fn a_status_change_and_a_split_on_a_dev_pane_reach_the_event_subscriber() {
    if !live_enabled() {
        return;
    }
    let label = assert_dev_session();
    let workspaces = herdr_ok(&["workspace", "list"]);
    let workspace = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["label"] == label.as_str())
        .unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let cwd = tempfile::tempdir().unwrap();
    let created = herdr_ok(&[
        "tab",
        "create",
        "--workspace",
        &workspace,
        "--cwd",
        cwd.path().to_str().unwrap(),
        "--label",
        &format!("lh-live-events-{}", std::process::id()),
        "--no-focus",
    ]);
    let _cleanup = CloseTabId(created["result"]["tab"]["tab_id"].as_str().unwrap().into());
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (tx, rx) = std::sync::mpsc::channel();
    use events::{Pushed, SocketSubscriber, Subscriber, Subscription};
    let _connection = SocketSubscriber::new(Some(dev_socket()))
        .subscribe(
            &[
                Subscription::PaneCreated,
                Subscription::PaneClosed,
                Subscription::AgentDetected,
                Subscription::AgentStatusChanged(pane.clone()),
            ],
            Box::new(move |pushed| {
                let _ = tx.send(pushed);
            }),
        )
        .unwrap();
    let out = dev("herdr")
        .args([
            "pane",
            "report-agent",
            "--source",
            "loopherd-live",
            "--agent",
            "claude",
            "--state",
            "working",
            &pane,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let within = |secs| rx.recv_timeout(std::time::Duration::from_secs(secs));
    assert_eq!(within(5), Ok(Pushed::Event), "no status event");
    while within(1).is_ok() {}
    herdr_ok(&[
        "pane",
        "split",
        "--pane",
        &pane,
        "--direction",
        "right",
        "--no-focus",
    ]);
    assert_eq!(within(5), Ok(Pushed::Event), "no pane.created event");
}

/// herdr with a throwaway home and a socket nothing listens on. Plugins
/// register per user, and a reachable server, the dev one included, would
/// register a link in its own home: the human's.
fn isolated_herdr(home: &Path) -> Command {
    let socket = home.join("no-server.sock");
    assert!(!socket.exists());
    herdr_in(home, &socket)
}

/// herdr with `home` as its whole home and `socket` as its server.
fn herdr_in(home: &Path, socket: &Path) -> Command {
    let mut cmd = Command::new("herdr");
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("HERDR_SOCKET_PATH", socket);
    let inherited = [
        "HERDR_CONFIG_PATH",
        "HERDR_SESSION",
        "HERDR_CLIENT_SOCKET_PATH",
    ];
    for var in HERDR_IDS.iter().chain(&inherited) {
        cmd.env_remove(var);
    }
    cmd
}

#[test]
fn plugin_links_enabled_into_an_isolated_herdr_home() {
    if !live_enabled() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin");

    let linked = isolated_herdr(home.path())
        .args(["plugin", "link"])
        .arg(&plugin)
        .output()
        .unwrap();
    assert!(linked.status.success(), "{linked:?}");
    assert!(home.path().join(".config/herdr/plugins.json").exists());

    let list = isolated_herdr(home.path())
        .args(["plugin", "list", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success(), "{list:?}");
    let plugins = json(&list)["result"]["plugins"].clone();
    let [entry] = plugins.as_array().unwrap().as_slice() else {
        panic!("expected one plugin: {plugins}");
    };
    assert_eq!(entry["plugin_id"], "loopherd");
    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        entry["plugin_root"],
        plugin.canonicalize().unwrap().to_str().unwrap()
    );
}

/// A herdr server of its own, run with a throwaway home so a plugin linked
/// through it registers there, unlike through the dev server, which runs as
/// the human. Its panes find the built `loopherd` first on `PATH`. Stopped on drop.
struct IsolatedServer {
    home: tempfile::TempDir,
    socket: PathBuf,
    server: std::process::Child,
}

impl IsolatedServer {
    fn start() -> Self {
        // Short, so the socket path fits in sun_path.
        let home = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let socket = home.path().join("herdr.sock");
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_loopherd"), bin.join("loopherd")).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let server = herdr_in(home.path(), &socket)
            .arg("server")
            .env("PATH", path)
            .env("LOOPHERD_CONFIG_DIR", home.path().join("loopherd-config"))
            .env_remove("CLAUDE_PID")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let isolated = IsolatedServer {
            home,
            socket,
            server,
        };
        let up = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            isolated
                .herdr()
                .args(["workspace", "list"])
                .output()
                .unwrap()
                .status
                .success()
        });
        assert!(up, "isolated herdr server did not start");
        isolated
    }

    fn herdr(&self) -> Command {
        herdr_in(self.home.path(), &self.socket)
    }

    fn ok(&self, args: &[&str]) {
        let out = self.herdr().args(args).output().unwrap();
        assert!(out.status.success(), "herdr {args:?}: {out:?}");
    }

    /// Sends one raw socket request; herdr's CLI has no `popup.close`.
    fn request(&self, method: &str) -> Value {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket).unwrap();
        let request = serde_json::json!({"id": "live", "method": method, "params": {}});
        writeln!(stream, "{request}").unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap()
    }

    /// The pids of built `loopherd` processes this server started, and their
    /// environment.
    fn loopherds(&self) -> Vec<(u32, Vec<(String, String)>)> {
        let built = Path::new(env!("CARGO_BIN_EXE_loopherd"))
            .canonicalize()
            .unwrap();
        let socket = self.socket.to_str().unwrap();
        let mut found = vec![];
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if std::fs::read_link(entry.path().join("exe")).ok().as_deref() != Some(&built) {
                continue;
            }
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            let env: Vec<(String, String)> = String::from_utf8_lossy(&environ)
                .split('\0')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if env
                .iter()
                .any(|(k, v)| k == "HERDR_SOCKET_PATH" && v == socket)
            {
                found.push((pid, env));
            }
        }
        found
    }

    fn wait_for_loopherds(&self, count: usize) -> Vec<(u32, Vec<(String, String)>)> {
        for _ in 0..50 {
            let found = self.loopherds();
            if found.len() == count {
                return found;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "expected {count} loopherd processes: {:?}",
            self.loopherds()
        );
    }
}

impl Drop for IsolatedServer {
    fn drop(&mut self) {
        // A view whose pty hangs up spins instead of exiting, so end it first.
        let signal_all = |signal| {
            for (pid, _) in self.loopherds() {
                // SAFETY: kill takes no pointers.
                unsafe { libc::kill(pid as i32, signal) };
            }
        };
        signal_all(libc::SIGTERM);
        for _ in 0..20 {
            if self.loopherds().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        signal_all(libc::SIGKILL);
        let _ = self.herdr().args(["server", "stop"]).output();
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// How a test ends the view popup.
enum Close {
    /// The view exits by itself: on SIGTERM it restores the terminal and exits 143.
    ViewQuits,
    /// herdr closes the popup.
    PopupClose,
}

/// Opens the view popup with `open`, checks it runs the view as a popup
/// does, closes it with `close`, and waits for loopherd and the popup to be gone.
fn view_popup_opens_and_closes(open: &[&str], close: Close) {
    let server = IsolatedServer::start();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin");
    server.ok(&["plugin", "link", plugin.to_str().unwrap()]);
    server.ok(&[
        "workspace",
        "create",
        "--cwd",
        server.home.path().to_str().unwrap(),
    ]);

    server.ok(open);

    let [(pid, env)] = server.wait_for_loopherds(1).try_into().unwrap();
    let var = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(var("HERDR_PANE_ID"), None);
    assert_eq!(var("HERDR_PLUGIN_ENTRYPOINT_ID"), Some("view"));
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(cmdline.ends_with(b"\0view\0"), "{cmdline:?}");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(server.loopherds().len(), 1, "the view exited on its own");

    match close {
        Close::ViewQuits => {
            // SAFETY: kill takes no pointers.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        Close::PopupClose => {
            assert_eq!(server.request("popup.close")["result"]["type"], "ok");
        }
    }

    server.wait_for_loopherds(0);
    assert_eq!(
        server.request("popup.close")["error"]["code"],
        "popup_not_open"
    );
}

#[test]
fn the_view_pane_opens_as_a_popup_that_closes_when_the_view_quits() {
    if !live_enabled() {
        return;
    }
    view_popup_opens_and_closes(
        &[
            "plugin",
            "pane",
            "open",
            "--plugin",
            "loopherd",
            "--entrypoint",
            "view",
        ],
        Close::ViewQuits,
    );
}

#[test]
fn the_view_action_opens_the_popup_and_popup_close_ends_the_view() {
    if !live_enabled() {
        return;
    }
    view_popup_opens_and_closes(
        &["plugin", "action", "invoke", "loopherd.view"],
        Close::PopupClose,
    );
}

/// herdr can't read a popup's screen, so this checks the card's process:
/// it runs with the member and PR herdr passed, waits for a key, and goes
/// when the popup closes. `pr_link`'s tests check the card's text.
#[test]
fn the_pr_popup_opens_with_the_member_and_waits_until_closed() {
    if !live_enabled() {
        return;
    }
    let server = IsolatedServer::start();
    let state = server.home.path().join(".local/state/loopherd");
    std::fs::create_dir_all(&state).unwrap();
    let ledger = r#"{"lh-1": {
        "name": "lh-1", "pane_id": "w1:p1", "session_id": "abc",
        "repo": "/src/repo", "worktree": null, "branch": "feat/lh-1",
        "workspace": "w1", "claude_args": [], "prompt": null,
        "prompt_sent": true, "created": "2026-10-31T00:00:00Z",
        "retired": false, "herd": "loopherd"
    }}"#;
    std::fs::write(state.join("ledger.json"), ledger).unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin");
    server.ok(&["plugin", "link", plugin.to_str().unwrap()]);
    server.ok(&[
        "workspace",
        "create",
        "--cwd",
        server.home.path().to_str().unwrap(),
    ]);
    let pr = "https://github.com/o/r/pull/12";

    server.ok(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "loopherd",
        "--entrypoint",
        "pr-member",
        "--env",
        "LOOPHERD_PR_MEMBER=lh-1",
        "--env",
        &format!("LOOPHERD_PR_URL={pr}"),
    ]);

    let [(pid, env)] = server.wait_for_loopherds(1).try_into().unwrap();
    let var = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(var("HERDR_PLUGIN_ENTRYPOINT_ID"), Some("pr-member"));
    assert_eq!(var("LOOPHERD_PR_MEMBER"), Some("lh-1"));
    assert_eq!(var("LOOPHERD_PR_URL"), Some(pr));
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    assert!(cmdline.ends_with(b"\0pr\0--popup\0"), "{cmdline:?}");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(server.loopherds().len(), 1, "the popup closed on its own");

    assert_eq!(server.request("popup.close")["result"]["type"], "ok");
    server.wait_for_loopherds(0);
}

impl IsolatedServer {
    fn json(&self, args: &[&str]) -> Value {
        let out = self.herdr().args(args).output().unwrap();
        assert!(out.status.success(), "herdr {args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The pid and env of the `loopherd view` the plugin's view pane runs,
    /// once it starts.
    fn view(&self) -> (u32, Vec<(String, String)>) {
        for _ in 0..50 {
            if let Some(view) = self.loopherds().into_iter().find(|(_, env)| {
                env.iter()
                    .any(|(k, v)| k == "HERDR_PLUGIN_ENTRYPOINT_ID" && v == "view")
            }) {
                return view;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("no view started: {:?}", self.loopherds());
    }
}

impl IsolatedServer {
    /// Runs `command` in `pane` and returns the pid and start time of the
    /// shell's child whose argv is `command`'s words, standing in for a Claude.
    fn run_in(&self, pane: &str, command: &str) -> (u32, u64) {
        let info = self.json(&["pane", "process-info", "--pane", pane]);
        let shell = info["result"]["process_info"]["shell_pid"]
            .as_u64()
            .unwrap();
        let argv: Vec<&str> = command.split_whitespace().collect();
        self.ok(&["pane", "run", pane, command]);
        for _ in 0..50 {
            for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                let Some((_, fields)) = stat.rsplit_once(')') else {
                    continue;
                };
                let fields: Vec<&str> = fields.split_whitespace().collect();
                if fields[1].parse::<u64>().ok() != Some(shell) {
                    continue;
                }
                let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
                let args: Vec<&[u8]> = cmdline
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .collect();
                if args.iter().copied().eq(argv.iter().map(|a| a.as_bytes())) {
                    let pid = entry.file_name().to_string_lossy().parse().unwrap();
                    return (pid, fields[19].parse().unwrap());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("no `{command}` started under pane {pane}'s shell {shell}");
    }
}

/// A workspace split into `w1:p1`, focused, and `w1:p2` right of it.
fn two_panes(server: &IsolatedServer) {
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin");
    server.ok(&["plugin", "link", plugin.to_str().unwrap()]);
    server.ok(&[
        "workspace",
        "create",
        "--cwd",
        server.home.path().to_str().unwrap(),
    ]);
    server.ok(&[
        "pane",
        "split",
        "w1:p1",
        "--direction",
        "right",
        "--no-focus",
    ]);
}

/// Each pane in the active tab, left to right, with its width and focus.
fn panes_across(server: &IsolatedServer) -> Vec<(String, u64, bool)> {
    let layout = server.json(&["pane", "layout"]);
    let mut panes: Vec<(u64, String, u64, bool)> = layout["result"]["layout"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["rect"]["x"].as_u64().unwrap(),
                p["pane_id"].as_str().unwrap().to_string(),
                p["rect"]["width"].as_u64().unwrap(),
                p["focused"].as_bool().unwrap(),
            )
        })
        .collect();
    panes.sort();
    panes.into_iter().map(|(_, id, w, f)| (id, w, f)).collect()
}

fn var<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[test]
fn the_beside_shepherd_action_splits_the_view_right_of_the_recorded_shepherd() {
    if !live_enabled() {
        return;
    }
    let server = IsolatedServer::start();
    two_panes(&server);
    let state = server.home.path().join(".local/state/loopherd");
    std::fs::create_dir_all(&state).unwrap();
    let (pid, start) = server.run_in("w1:p2", "sleep 300");
    let record = serde_json::json!({
        "pane": "w1:p2", "socket": server.socket, "process": {"pid": pid, "start": start},
        "session": "s-1", "at": "2026-11-19T00:00:00Z", "used": true,
    });
    std::fs::write(state.join("shepherd.json"), record.to_string()).unwrap();

    server.ok(&[
        "plugin",
        "action",
        "invoke",
        "loopherd.view-beside-shepherd",
    ]);

    let (pid, env) = server.view();
    assert_eq!(var(&env, "LOOPHERD_VIEW_NOTE"), None);
    let panes = panes_across(&server);
    let [
        (p1, _, true),
        (shepherd, shepherd_w, false),
        (_, view_w, false),
    ] = panes.as_slice()
    else {
        panic!("{panes:?}");
    };
    assert_eq!((p1.as_str(), shepherd.as_str()), ("w1:p1", "w1:p2"));
    assert!(view_w < shepherd_w, "{panes:?}");

    // SAFETY: kill takes no pointers.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    server.wait_for_loopherds(0);
    for _ in 0..20 {
        if panes_across(&server).len() == 2 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!(
        "the view's pane outlived the view: {:?}",
        panes_across(&server)
    );
}

#[test]
fn with_no_shepherd_recorded_the_action_splits_right_of_the_focused_pane_saying_why() {
    if !live_enabled() {
        return;
    }
    let server = IsolatedServer::start();
    two_panes(&server);

    server.ok(&[
        "plugin",
        "action",
        "invoke",
        "loopherd.view-beside-shepherd",
    ]);

    let (_, env) = server.view();
    let note = var(&env, "LOOPHERD_VIEW_NOTE").unwrap();
    assert!(note.contains("no shepherd recorded"), "{note}");
    let panes = panes_across(&server);
    let ids: Vec<&str> = panes.iter().map(|(id, _, _)| id.as_str()).collect();
    assert_eq!(ids[0], "w1:p1");
    assert_eq!(ids[2], "w1:p2", "{panes:?}");
    assert!(panes[0].2, "focus left w1:p1: {panes:?}");
}

const LIVE: &str = "/home/u/.config/herdr/herdr.sock";

#[test]
fn guard_accepts_the_dev_socket_set_by_herdr_dev() {
    let dev = Path::new("/home/u/.config/herdr/sessions/loopherd-dev/herdr.sock");
    assert_eq!(
        check_inherited_socket(Some(dev), dev, Path::new(LIVE)),
        Ok(())
    );
}

#[test]
fn guard_rejects_an_inherited_live_socket() {
    let dev = Path::new("/home/u/.config/herdr/sessions/loopherd-dev/herdr.sock");
    let live = Path::new(LIVE);
    assert!(check_inherited_socket(Some(live), dev, live).is_err());
}

#[test]
fn guard_rejects_a_run_outside_herdr_dev() {
    let dev = Path::new("/home/u/.config/herdr/sessions/loopherd-dev/herdr.sock");
    assert!(check_inherited_socket(None, dev, Path::new(LIVE)).is_err());
}

#[test]
fn guard_rejects_a_dev_socket_symlinked_to_the_live_socket() {
    let home = tempfile::tempdir().unwrap();
    let live = home.path().join(".config/herdr/herdr.sock");
    let dev = home
        .path()
        .join(".config/herdr/sessions/loopherd-dev/herdr.sock");
    std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
    std::os::unix::fs::symlink(&live, &dev).unwrap();

    let err = check_inherited_socket(Some(&dev), &dev, &live).unwrap_err();
    assert!(err.contains("live socket"), "{err}");
}

fn mailbox_lines(state: &Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(state.join(format!("loopherd/reports/{name}.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn stops(state: &Path, name: &str) -> Vec<Value> {
    mailbox_lines(state, name)
        .into_iter()
        .filter(|e| e["kind"] == "stop")
        .collect()
}

/// A member spawned into the dev session with Stop hooks from a `--settings`
/// file, recording into `state`; `live_state` holds the same pane under the
/// live herdr's socket, to show the hook keeps the two apart.
struct Hooked {
    state: tempfile::TempDir,
    live_state: tempfile::TempDir,
    name: String,
    _cleanup: CloseTab,
}

const SLEEPER: &str = "Run `sleep 30` with the Bash tool and run_in_background set to true. \
     Then end your turn at once: don't wait for it or check on it. \
     When it finishes, reply with just the word done.";

/// A member running [`SLEEPER`], denied the tools that could keep it paused
/// past its shells.
fn spawn_sleeper(tag: &str) -> Hooked {
    spawn_hooked_with(
        tag,
        SLEEPER,
        &["--disallowedTools", "CronCreate,ScheduleWakeup,Monitor"],
    )
}

/// `claude_args` go to the member's Claude after the `--settings` file.
fn spawn_hooked_with(tag: &str, prompt: &str, claude_args: &[&str]) -> Hooked {
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let live_state = tempfile::tempdir().unwrap();
    let name = format!("lh-live-{tag}-{}", std::process::id());
    let cleanup = CloseTab(name.clone());
    let hook = |state: &Path| {
        format!(
            "XDG_STATE_HOME='{}' '{}' stop-hook || true",
            state.display(),
            env!("CARGO_BIN_EXE_loopherd")
        )
    };
    let settings = state.path().join("settings.json");
    let session_hooks = serde_json::json!([{"hooks": [{
        "type": "command",
        "command": format!(
            "XDG_STATE_HOME='{}' '{}' session-hook || true",
            state.path().display(),
            env!("CARGO_BIN_EXE_loopherd")
        ),
        "timeout": 10,
    }]}]);
    let hooks = serde_json::json!([{"hooks": [
        {"type": "command", "command": hook(state.path()), "timeout": 10},
        {"type": "command", "command": hook(live_state.path()), "timeout": 10},
        {"type": "command", "command": format!("echo $CLAUDE_PID >> '{}'", state.path().join("stop-pids").display()), "timeout": 10},
    ]}]);
    std::fs::write(
        &settings,
        serde_json::json!({
            "hooks": {"Stop": hooks, "StopFailure": hooks, "SessionStart": session_hooks},
            "permissions": {"allow": ["Bash(sleep:*)", "Bash(claude:*)"]},
        })
        .to_string(),
    )
    .unwrap();

    let out = spawn(
        state.path(),
        &[
            "--repo",
            env!("CARGO_MANIFEST_DIR"),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--prompt",
            prompt,
        ],
        &[&["--settings", settings.to_str().unwrap()], claude_args].concat(),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let entry = ledger_entry(state.path(), &name);
    assert_eq!(entry["socket"], dev_socket().to_str().unwrap());
    assert_eq!(mailbox_lines(state.path(), &name)[0]["kind"], "start");

    let mut live_entry = entry.clone();
    live_entry["socket"] = live_socket().to_str().unwrap().into();
    std::fs::create_dir_all(live_state.path().join("loopherd")).unwrap();
    std::fs::write(
        live_state.path().join("loopherd/ledger.json"),
        serde_json::json!({ &name: live_entry }).to_string(),
    )
    .unwrap();
    Hooked {
        state,
        live_state,
        name,
        _cleanup: cleanup,
    }
}

impl Hooked {
    fn assert_live_state_untouched(&self) {
        assert!(
            !self.live_state.path().join("loopherd/reports").exists(),
            "the live-socket hook recorded a stop"
        );
    }
}

#[test]
fn the_stop_hook_records_a_paused_stop_then_a_finished_one() {
    if !live_enabled() {
        return;
    }
    let member = spawn_sleeper("stop");
    let state = member.state.path();

    // The member may start more than one shell, each ending in a paused stop.
    let finished =
        |s: &Value| s["background"] == serde_json::json!({"shells": 0, "subagents": 0, "other": 0});
    let stops = wait_for(state, &member.name, |stops| {
        stops.last().is_some_and(finished)
    });
    assert!(stops.len() >= 2, "{stops:?}");
    assert!(
        stops[0]["background"]["shells"].as_u64() >= Some(1),
        "{stops:?}"
    );
    assert!(stops.last().is_some_and(finished), "{stops:?}");
    assert_eq!(stops.last().unwrap()["crons"], 0, "{stops:?}");
    member.assert_live_state_untouched();
}

#[test]
fn watch_wakes_paused_then_a_rearmed_watch_wakes_finished() {
    if !live_enabled() {
        return;
    }
    let member = spawn_sleeper("watch");
    let state = member.state.path();
    // The member's hooks carry an XDG_STATE_HOME prefix, so give watch's
    // installed-hook check a plain one.
    let check = state.join("installed.json");
    let out = dev(env!("CARGO_BIN_EXE_loopherd"))
        .args(["install-hook", "--settings", check.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let watch = || {
        let out = dev(env!("CARGO_BIN_EXE_loopherd"))
            .env("XDG_STATE_HOME", state)
            .args([
                "watch",
                "--timeout",
                "180",
                "--settings",
                check.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let paused = watch();
    let pending = paused
        .strip_prefix(&format!("{} paused  ", member.name))
        .unwrap_or_else(|| panic!("{paused:?}"));
    assert!(pending.contains(" shell"), "{paused:?}");
    assert_eq!(watch(), format!("{} finished  no report\n", member.name));
    member.assert_live_state_untouched();
}

/// Polls `name`'s stops until `done` holds of them or four minutes pass.
fn wait_for(state: &Path, name: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    loop {
        let stops = stops(state, name);
        if done(&stops) || std::time::Instant::now() >= deadline {
            return stops;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn wait_for_stops(state: &Path, name: &str, count: usize) -> Vec<Value> {
    wait_for(state, name, |stops| stops.len() >= count)
}

fn say(name: &str, text: &str) {
    herdr_ok(&["agent", "prompt", name, text]);
}

#[test]
fn only_the_members_own_stops_are_filed_across_a_nested_claude_and_a_branch() {
    if !live_enabled() {
        return;
    }
    let member = spawn_hooked_with("branch", "Reply with just the word zero.", &[]);
    let state = member.state.path();
    let settings = state.join("settings.json");

    let first = wait_for_stops(state, &member.name, 1);
    assert_eq!(first.len(), 1, "{first:?}");
    let entry = ledger_entry(state, &member.name);
    assert!(entry["process"]["pid"].is_u64(), "unclaimed: {entry}");
    let session = entry["session_id"].clone();
    assert!(session.is_string(), "{session}");

    let nested_out = state.join("nested.out");
    say(
        &member.name,
        &format!(
            "Run `claude -p {} --resume {} --settings {} 'Reply with just ok.' > {}` \
             with the Bash tool and wait for it. Then reply with just the word one.",
            MODEL.join(" "),
            session.as_str().unwrap(),
            settings.display(),
            nested_out.display()
        ),
    );
    wait_for_stops(state, &member.name, 2);
    std::thread::sleep(std::time::Duration::from_secs(10));
    let all = stops(state, &member.name);
    assert_eq!(all.len(), 2, "a nested stop was filed: {all:?}");
    let nested = std::fs::read_to_string(&nested_out).unwrap_or_default();
    assert!(!nested.trim().is_empty(), "the nested claude never ran");
    let pids = std::fs::read_to_string(state.join("stop-pids")).unwrap();
    let distinct: std::collections::BTreeSet<_> = pids.split_whitespace().collect();
    assert_eq!(
        distinct.len(),
        2,
        "the nested claude's Stop hook never fired: {pids}"
    );

    say(&member.name, "/branch");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // The new session can show before Claude's input is ready for a prompt.
    let branched = || {
        let status =
            herdr_ok(&["agent", "get", &member.name])["result"]["agent"]["agent_status"].clone();
        ledger_entry(state, &member.name)["session_id"] != session
            && (status == "idle" || status == "done")
    };
    while !branched() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    say(&member.name, "Reply with just the word two.");

    let all = wait_for_stops(state, &member.name, 3);
    assert_eq!(all.len(), 3, "{all:?}");
    let branched = ledger_entry(state, &member.name)["session_id"].clone();
    assert_ne!(branched, session, "the ledger did not follow /branch");
    assert_eq!(all[2]["session"], branched, "{all:?}");
    member.assert_live_state_untouched();
}

#[test]
fn a_leads_question_labels_its_claude_pane_until_a_typed_prompt() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let name = format!("lh-live-label-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());
    let out = spawn(
        state.path(),
        &[
            "--repo",
            env!("CARGO_MANIFEST_DIR"),
            "--name",
            &name,
            "--workspace",
            &workspace,
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let ledger = state.path().join("loopherd/ledger.json");
    let mut entries: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    entries[&name]["role"] = "lead".into();
    entries[&name]["herd"] = "cards".into();
    std::fs::write(&ledger, entries.to_string()).unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        "[notify]\nenabled = true\nherdr = false\npane_label = true\n",
    )
    .unwrap();
    let pane = entries[&name]["pane_id"].as_str().unwrap().to_string();
    let in_pane = |args: &[&str]| {
        let mut cmd = dev(env!("CARGO_BIN_EXE_loopherd"));
        cmd.env("XDG_STATE_HOME", state.path())
            .env("LOOPHERD_CONFIG_DIR", config.path())
            .env("HERDR_PANE_ID", &pane)
            .args(args);
        cmd
    };
    let prompt_hook = |prompt: &str| {
        let input = serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt});
        let mut child = in_pane(&["prompt-hook"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    };
    let shown = || herdr_ok(&["pane", "get", &pane])["result"]["pane"].clone();

    let out = in_pane(&["report", "--kind", "question", "which DB?"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let labelled = shown();
    assert_eq!(labelled["display_agent"], format!("{name} asks: which DB?"));
    assert_ne!(labelled["agent_status"], "blocked");
    let listed = herdr_ok(&["agent", "list"])["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["pane_id"] == pane.as_str())
        .cloned()
        .unwrap();
    assert_eq!(listed["display_agent"], format!("{name} asks: which DB?"));
    assert_eq!(listed["name"], name.as_str());
    assert_ne!(listed["agent_status"], "blocked");

    prompt_hook("<task-notification>\n<status>completed</status>\n</task-notification>");
    assert_eq!(shown()["display_agent"], format!("{name} asks: which DB?"));
    prompt_hook("use postgres");
    assert!(shown()["display_agent"].is_null(), "{}", shown());
}

fn poll<T>(secs: u64, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Runs a herdr command that prints nothing on success.
fn herdr_quiet(args: &[&str]) {
    let out = dev("herdr").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "herdr {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn visible(pane: &str) -> String {
    let out = dev("herdr")
        .args(["pane", "read", pane, "--source", "visible"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn capital_s_in_a_view_pane_starts_the_shepherd_below_it_then_focuses_it() {
    if !live_enabled() {
        return;
    }
    let label = assert_dev_session();
    let shepherd_before = dev("herdr").args(["agent", "get", "shepherd"]).output();
    assert!(
        !shepherd_before.unwrap().status.success(),
        "loopherd-dev already has an agent named shepherd; close it first"
    );
    let workspaces = herdr_ok(&["workspace", "list"]);
    let workspace = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["label"] == label.as_str())
        .unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (state, config) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let tab = herdr_ok(&[
        "tab",
        "create",
        "--workspace",
        &workspace,
        "--cwd",
        env!("CARGO_MANIFEST_DIR"),
        "--label",
        &format!("lh-live-shepherd-{}", std::process::id()),
        "--no-focus",
    ]);
    let _cleanup = CloseTabId(tab["result"]["tab"]["tab_id"].as_str().unwrap().into());
    let view = tab["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    poll(15, || (!visible(&view).trim().is_empty()).then_some(())).expect("no shell prompt");
    let command = format!(
        "XDG_STATE_HOME={} LOOPHERD_CONFIG_DIR={} {} view",
        state.path().display(),
        config.path().display(),
        env!("CARGO_BIN_EXE_loopherd"),
    );
    herdr_quiet(&["pane", "run", &view, &command]);
    poll(15, || visible(&view).contains("q quit").then_some(())).expect("no view drawn");

    herdr_quiet(&["pane", "send-keys", &view, "S"]);

    let shepherd = poll(90, || {
        let out = dev("herdr")
            .args(["agent", "get", "shepherd"])
            .output()
            .ok()?;
        out.status.success().then(|| json(&out))
    })
    .expect("no shepherd started");
    let below = shepherd["result"]["agent"]["pane_id"].as_str().unwrap();
    let layout = herdr_ok(&["pane", "layout", "--pane", &view]);
    let rect = |pane: &str| {
        let panes = layout["result"]["layout"]["panes"].as_array().unwrap();
        panes.iter().find(|p| p["pane_id"] == pane).unwrap()["rect"].clone()
    };
    let (top, bottom) = (rect(&view), rect(below));
    assert_eq!(top["x"], bottom["x"], "{layout}");
    assert!(
        bottom["y"].as_u64() > top["y"].as_u64(),
        "shepherd not below the view: {layout}"
    );
    poll(90, || {
        visible(&view)
            .contains("started the shepherd")
            .then_some(())
    })
    .unwrap_or_else(|| panic!("{}", visible(&view)));

    herdr_quiet(&["pane", "send-keys", &view, "S"]);

    poll(15, || {
        visible(&view)
            .contains("focused the shepherd")
            .then_some(())
    })
    .unwrap_or_else(|| panic!("{}", visible(&view)));
    let panes = herdr_ok(&["pane", "list", "--workspace", &workspace]);
    let in_tab = panes["result"]["panes"].as_array().unwrap().iter();
    let tab_id = &tab["result"]["tab"]["tab_id"];
    assert_eq!(
        in_tab.filter(|p| p["tab_id"] == *tab_id).count(),
        2,
        "{panes}"
    );
}
