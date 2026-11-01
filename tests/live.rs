//! End-to-end runs of `loopherd spawn`, `add`, `revive` and `retire --close` against the real `loopherd-dev` herdr
//! session. Skipped unless `LOOPHERD_LIVE=1`; start the session first with
//! `scripts/herdr-dev --start`, then run through it:
//! `LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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

/// A command whose herdr calls reach only the dev session.
fn dev(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env("HERDR_SOCKET_PATH", dev_socket());
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

fn spawn(state: &Path, args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .arg("spawn")
        .args(args)
        .output()
        .unwrap()
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
        .args(["retire", name, "--close"])
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
            "repo = {:?}\nworkspace = {workspace:?}\n\
             [template.echo]\nparams = [\"n\"]\nname = \"lh-live-add-{{n}}\"\n\
             prompt = \"You're {{name}} in {{herd}}. Reply with just the word ok.\"\n",
            env!("CARGO_MANIFEST_DIR")
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
